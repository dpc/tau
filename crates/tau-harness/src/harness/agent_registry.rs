//! Owns live agent identity, creation, metadata, statistics, navigation, and
//! unload.
//!
//! Watch fanout remains a separate authority from registry membership.

use super::start_coordinator::{
    MAX_START_QUERY_ID_BYTES, StartCoordinator, StartPhase, StartPhaseOwner,
};
use super::*;

/// Agent identity, membership, routing, and lifecycle state owned by the
/// harness.
pub(crate) struct AgentRegistryState {
    /// All live agent runtimes keyed by stable agent id.
    pub(crate) agents: HashMap<AgentId, Agent>,
    /// Public agent id to current-session conversation route.
    pub(crate) agent_routes: HashMap<AgentId, AgentId>,
    /// Next process-local loaded-agent runtime incarnation.
    pub(super) next_runtime_incarnation: u64,
    /// Next process-local opaque agent-initialization correlation.
    pub(super) next_initialization_id: u64,
    /// Random identity stamped on outer turns authored by this harness runtime.
    pub(super) accounting_runtime_id: tau_proto::AccountingRuntimeId,
    /// Random stream used by agent-id template expansion.
    pub(super) id_rng: StdRng,
    /// Authenticated creator relationships for the active session.
    pub(super) creator_topology: AgentCreatorTopology,
    /// Runtime-only self and creator-subtree estimated-cost totals.
    pub(super) cost_ledger: AgentCostLedger,
    /// Creation facts committed before their normal publish pipeline.
    pub(super) precommitted_starts: HashSet<String>,
    /// Agent ids loaded or awaiting a must-pass membership publication.
    pub(crate) session_loaded: HashSet<AgentId>,
    /// Agent ids that have appeared in current-session membership history.
    pub(crate) session_ever_loaded: HashSet<AgentId>,
    /// Successfully committed current membership used by roster snapshots.
    pub(super) roster_loaded: HashSet<AgentId>,
    /// Successfully committed membership history used by roster snapshots.
    pub(super) roster_ever_loaded: HashSet<AgentId>,
    /// Durable membership history eligible for journal-backed accounting
    /// restore.
    pub(super) roster_durable_ever_loaded: HashSet<AgentId>,
    /// Whether restored and newly committed roster membership remains valid.
    pub(super) roster_valid: bool,
    /// Harness-owned navigation classification for loaded agents.
    pub(crate) navigation_modes: HashMap<AgentId, tau_proto::AgentNavigationMode>,
    /// Agent ids that were once known but can no longer receive messages.
    pub(crate) stopped_ids: HashSet<AgentId>,
    /// Restored members whose pre-restart request route is unavailable.
    pub(crate) restored_unavailable: HashMap<AgentId, String>,
    /// Outstanding built-in delegation query to child correlation.
    pub(crate) pending_builtin_delegates: HashMap<String, AgentId>,
    /// Extension-started agents waiting for ordered creation and dispatch.
    pub(super) pending_start_requests: VecDeque<PendingStartAgentRequest>,
    /// Bounded runtime owner for accepted multi-event startup obligations.
    pub(super) start_coordinator: StartCoordinator,
}

pub(super) fn agent_runtime_state_for_turn(state: &AgentTurnState) -> tau_proto::AgentRuntimeState {
    match state {
        AgentTurnState::Idle => tau_proto::AgentRuntimeState::Idle,
        AgentTurnState::AgentThinking { .. } | AgentTurnState::ToolsRunning { .. } => {
            tau_proto::AgentRuntimeState::Running
        }
    }
}

pub(super) fn default_navigation_mode(
    originator: &tau_proto::PromptOriginator,
) -> tau_proto::AgentNavigationMode {
    if matches!(originator, tau_proto::PromptOriginator::Extension { .. }) {
        tau_proto::AgentNavigationMode::ActiveAuto
    } else {
        tau_proto::AgentNavigationMode::Active
    }
}

/// Agent creation accepted by the harness but not yet installed as a live
/// route.
#[derive(Clone, Debug)]
pub(super) struct PendingStartAgentRequest {
    /// Connection that owns the accepted start request.
    pub(super) source_id: tau_proto::ConnectionId,
    /// Stable extension name used for lifecycle and result correlation.
    pub(super) extension_name: tau_proto::ExtensionName,
    /// Canonical accepted request payload.
    pub(super) query: tau_proto::StartAgentRequest,
    /// Resolved role captured at admission.
    pub(super) role: String,
    /// Reserved runtime conversation ID.
    pub(super) cid: AgentId,
    /// Optional runtime parent conversation.
    pub(super) parent_cid: Option<AgentId>,
    /// Reserved durable public agent ID.
    pub(super) agent_id: String,
    /// Persistence mode captured before installation.
    pub(super) persistence: tau_core::AgentPersistenceMode,
    /// Committed wakes accepted before installation and transferred exactly
    /// once to the live agent without duplicating payload authority.
    pub(super) pending_agent_message_wakes: VecDeque<crate::agent::PendingMessageWake>,
}

pub(super) const DEFAULT_AGENT_ID_TEMPLATE: &str = "{{random_alphanumeric 6}}";
pub(super) const AGENT_ID_TEMPLATE_COLLISION_ATTEMPTS: usize = 10;

pub(super) fn normalize_display_name(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
pub(super) fn deterministic_agent_id_rng() -> StdRng {
    StdRng::seed_from_u64(0)
}

pub(super) fn random_alphanumeric(len: usize, rng: &mut StdRng) -> String {
    use rand::Rng as _;

    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..len)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

pub(super) struct RandomAlphanumericHelper<'a> {
    collision_extra_len: usize,
    rng: Mutex<&'a mut StdRng>,
}

impl handlebars::HelperDef for RandomAlphanumericHelper<'_> {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &handlebars::Helper<'rc>,
        _: &'reg handlebars::Handlebars<'reg>,
        _: &'rc handlebars::Context,
        _: &mut handlebars::RenderContext<'reg, 'rc>,
    ) -> Result<handlebars::ScopedJson<'rc>, handlebars::RenderError> {
        let requested = h
            .param(0)
            .and_then(|param| param.value().as_u64())
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(6);
        let mut rng = self.rng.lock().expect("agent id rng lock poisoned");
        Ok(handlebars::ScopedJson::Derived(serde_json::Value::String(
            random_alphanumeric(requested.saturating_add(self.collision_extra_len), &mut rng),
        )))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AgentIdTemplateKind {
    Configured,
    Default,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum AgentIdMintWarning {
    RenderFailed { error: String },
    InvalidRendered { candidate: String, error: String },
    CollisionsExceeded { attempts: usize },
}

pub(super) fn handlebars_for_agent_template<'a>(
    collision_extra_len: usize,
    rng: &'a mut StdRng,
) -> handlebars::Handlebars<'a> {
    let mut handlebars = handlebars::Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars.register_escape_fn(handlebars::no_escape);
    handlebars.register_helper(
        "random_alphanumeric",
        Box::new(RandomAlphanumericHelper {
            collision_extra_len,
            rng: Mutex::new(rng),
        }),
    );
    handlebars
}

pub(super) fn base_agent_template_context(
    role: &str,
    role_group: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut context = serde_json::Map::new();
    context.insert(
        "role".to_owned(),
        serde_json::Value::String(role.to_owned()),
    );
    context.insert(
        "role_group".to_owned(),
        serde_json::Value::String(role_group.to_owned()),
    );
    context.insert(
        "roleGroup".to_owned(),
        serde_json::Value::String(role_group.to_owned()),
    );
    context
}

pub(super) fn render_agent_template(
    template: &str,
    role: &str,
    role_group: &str,
    agent_id: &str,
    task_name: Option<&str>,
    collision_extra_len: usize,
    rng: &mut StdRng,
) -> Result<String, handlebars::RenderError> {
    let handlebars = handlebars_for_agent_template(collision_extra_len, rng);
    let mut context = base_agent_template_context(role, role_group);
    context.insert(
        "agent_id".to_owned(),
        serde_json::Value::String(agent_id.to_owned()),
    );
    context.insert(
        "agentId".to_owned(),
        serde_json::Value::String(agent_id.to_owned()),
    );
    context.insert(
        "task_name".to_owned(),
        serde_json::Value::String(task_name.unwrap_or("").to_owned()),
    );
    context.insert(
        "taskName".to_owned(),
        serde_json::Value::String(task_name.unwrap_or("").to_owned()),
    );
    context.insert(
        "task_name_present".to_owned(),
        serde_json::Value::Bool(task_name.is_some()),
    );
    context.insert(
        "taskNamePresent".to_owned(),
        serde_json::Value::Bool(task_name.is_some()),
    );
    handlebars.render_template(template, &serde_json::Value::Object(context))
}

pub(super) fn render_agent_id_template(
    template: &str,
    role: &str,
    role_group: &str,
    collision_extra_len: usize,
    rng: &mut StdRng,
) -> Result<String, handlebars::RenderError> {
    let handlebars = handlebars_for_agent_template(collision_extra_len, rng);
    handlebars.render_template(
        template,
        &serde_json::Value::Object(base_agent_template_context(role, role_group)),
    )
}

pub(super) fn mint_available_agent_id_for_role_with(
    role: &str,
    role_group: &str,
    template: &str,
    mut is_taken: impl FnMut(&str) -> bool,
    rng: &mut StdRng,
    mut warn: impl FnMut(AgentIdTemplateKind, AgentIdMintWarning),
) -> String {
    let mut use_default = false;
    loop {
        let active_template = if use_default {
            DEFAULT_AGENT_ID_TEMPLATE
        } else {
            template
        };
        let kind = if use_default {
            AgentIdTemplateKind::Default
        } else {
            AgentIdTemplateKind::Configured
        };
        let max_attempts = if use_default {
            tau_proto::AGENT_ID_MAX_LEN
        } else {
            AGENT_ID_TEMPLATE_COLLISION_ATTEMPTS
        };
        let mut exhausted_attempts = true;
        for attempt in 0..max_attempts {
            let rendered =
                match render_agent_id_template(active_template, role, role_group, attempt, rng) {
                    Ok(rendered) => rendered,
                    Err(error) => {
                        warn(
                            kind,
                            AgentIdMintWarning::RenderFailed {
                                error: error.to_string(),
                            },
                        );
                        exhausted_attempts = false;
                        break;
                    }
                };
            let agent_id = match rendered.parse::<AgentId>() {
                Ok(agent_id) => agent_id,
                Err(error) => {
                    warn(
                        kind,
                        AgentIdMintWarning::InvalidRendered {
                            candidate: rendered,
                            error: error.to_string(),
                        },
                    );
                    exhausted_attempts = false;
                    break;
                }
            };
            if !is_taken(agent_id.as_str()) {
                return agent_id.into_string();
            }
        }
        if use_default {
            panic!("unable to mint unique agent id with default template");
        }
        if exhausted_attempts {
            warn(
                AgentIdTemplateKind::Configured,
                AgentIdMintWarning::CollisionsExceeded {
                    attempts: AGENT_ID_TEMPLATE_COLLISION_ATTEMPTS,
                },
            );
        }
        use_default = true;
    }
}

#[cfg(test)]
pub(super) fn mint_agent_id_for_role(role: &str) -> String {
    let mut rng = deterministic_agent_id_rng();
    mint_available_agent_id_for_role_with(
        role,
        role,
        DEFAULT_AGENT_ID_TEMPLATE,
        |_| false,
        &mut rng,
        |_, _| {},
    )
}

impl Harness {
    pub(crate) fn agent_display_name_for_cid(&self, cid: &AgentId) -> Option<String> {
        self.agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|conv| {
                normalize_display_name(conv.identity.display_name.as_deref())
                    .or_else(|| conv.identity.agent_id.as_ref().map(ToString::to_string))
            })
    }

    pub(super) fn available_delegate_role_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .config
            .available_roles
            .keys()
            .filter(|name| {
                model_for_role(
                    &self.provider_runtime.model_info,
                    &self.config.available_roles,
                    name,
                )
                .is_some()
            })
            .cloned()
            .collect();
        names.sort();
        names
    }

    pub(super) fn available_delegate_roles_message(&self) -> String {
        let roles = self.available_delegate_role_names();
        if roles.is_empty() {
            "available roles: (none)".to_owned()
        } else {
            format!("available roles: {}", roles.join(", "))
        }
    }

    pub(super) fn resolve_start_agent_request_role(
        &self,
        query: &tau_proto::StartAgentRequest,
    ) -> Result<String, String> {
        let requested = if let Some(role) = query.role.as_deref() {
            role
        } else if query.tool_call_id.is_some() {
            "engineer"
        } else {
            self.config.selected_role.as_str()
        };

        if self.config.available_roles.contains_key(requested)
            && model_for_role(
                &self.provider_runtime.model_info,
                &self.config.available_roles,
                requested,
            )
            .is_some()
        {
            return Ok(requested.to_owned());
        }

        let reason = if query.role.is_none() && query.tool_call_id.is_some() {
            "agent_start requires default role `engineer`, but it is not available"
        } else if self.config.available_roles.contains_key(requested) {
            "requested role is not backed by an available model"
        } else if let Some(reason) = self.config.disabled_role_reasons.get(requested) {
            return Err(format!(
                "requested role is disabled by configuration: {}; {}",
                reason.message,
                self.available_delegate_roles_message()
            ));
        } else {
            "requested role does not exist"
        };
        Err(format!(
            "{reason}: `{requested}`; {}",
            self.available_delegate_roles_message()
        ))
    }

    pub(super) fn fail_start_agent_request(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        query_id: String,
        error: String,
    ) {
        let result = tau_proto::StartAgentResult {
            query_id,
            text: String::new(),
            error: Some(error),
        };
        if source_id == harness_connection_id() {
            self.publish_event(
                Some(crate::harness::harness_connection_id()),
                Event::StartAgentResult(result),
            );
        } else {
            let _ = self.runtime_io.bus.send_to(
                source_id,
                None,
                HarnessOutputMessage::deliver(Event::StartAgentResult(result)),
            );
        }
    }

    /// Queue and dispatch an extension-started sub-agent request.
    pub(super) fn handle_start_agent_request(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        query: tau_proto::StartAgentRequest,
    ) -> Result<(), HarnessError> {
        let query_id = query.query_id.clone();
        let pending = match self.prepare_start_agent_request(source_id, query) {
            Ok(Some(pending)) => pending,
            Ok(None) => return Ok(()),
            Err(error) => {
                self.fail_start_agent_request(source_id, query_id, error);
                return Ok(());
            }
        };
        self.agent_runtime
            .agent_registry
            .pending_start_requests
            .push_back(pending);
        self.drain_pending_start_agent_requests()
    }

    pub(super) fn accept_duplicate_start_agent_request(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        query_id: &str,
        agent_id: &str,
        start_id: tau_proto::StartOperationId,
    ) {
        let accepted = tau_proto::StartAgentAccepted {
            start_id,
            query_id: query_id.to_owned(),
            agent_id: crate::parse_agent_id(agent_id),
        };
        let _ = self.runtime_io.bus.send_to(
            source_id,
            None,
            HarnessOutputMessage::deliver(Event::StartAgentAccepted(accepted)),
        );
    }

    /// Enqueue an internal start-agent request and return its minted agent id.
    pub(crate) fn enqueue_internal_start_agent_request_without_draining(
        &mut self,
        query: tau_proto::StartAgentRequest,
    ) -> Result<String, String> {
        let Some(pending) =
            self.prepare_start_agent_request(crate::harness::harness_connection_id(), query)?
        else {
            return Err("duplicate tool-backed start-agent request".to_owned());
        };
        let agent_id = pending.agent_id.clone();
        self.agent_runtime
            .agent_registry
            .pending_start_requests
            .push_back(pending);
        Ok(agent_id)
    }

    pub(super) fn validate_agent_metadata_key(
        &self,
        key: &tau_proto::AgentMetadataKey,
    ) -> Result<(), String> {
        if matches!(
            key.as_str(),
            path_crate_harness::subagents_tool::PEER_ENTRYPOINT_AGENT_METADATA_KEY
                | path_crate_harness::subagents_tool::BOOTSTRAP_PROMPT_AGENT_METADATA_KEY
        ) {
            return Err("agent metadata key is reserved for harness lifecycle state".to_owned());
        }
        if key.as_str().is_empty() {
            return Err("agent metadata key must not be empty".to_owned());
        }
        if tau_proto::MAX_AGENT_METADATA_KEY_BYTES < key.as_str().len() {
            return Err("agent metadata key exceeds 256 bytes".to_owned());
        }
        Ok(())
    }

    pub(super) fn validate_agent_metadata_target(
        &mut self,
        agent_id: &tau_proto::AgentId,
    ) -> Result<(), String> {
        if self
            .agent_runtime
            .agent_registry
            .session_loaded
            .contains(agent_id)
            || self
                .agent_runtime
                .agent_registry
                .agent_routes
                .contains_key(agent_id.as_str())
        {
            return Ok(());
        }
        match self
            .session_runtime
            .agent_store
            .load_agent(agent_id.as_str())
        {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(format!("unknown agent metadata target `{agent_id}`")),
            Err(error) => Err(format!(
                "failed to load metadata target `{agent_id}`: {error}"
            )),
        }
    }

    pub(super) fn validate_agent_metadata_set(
        &mut self,
        set: &tau_proto::AgentMetadataSet,
    ) -> Result<(), String> {
        self.validate_agent_metadata_target(&set.agent_id)?;
        self.validate_agent_metadata_key(&set.key)?;
        if set
            .mutation_id
            .as_ref()
            .is_some_and(|id| tau_proto::MAX_AGENT_METADATA_MUTATION_ID_BYTES < id.as_str().len())
        {
            return Err("agent metadata mutation id exceeds maximum size".to_owned());
        }
        let value_bytes = tau_proto::encode_message_to_vec(&set.value)
            .map_err(|error| format!("failed to measure agent metadata value: {error}"))?;
        if tau_proto::MAX_AGENT_METADATA_VALUE_BYTES < value_bytes.len() {
            return Err("agent metadata value exceeds 64 KiB".to_owned());
        }
        Ok(())
    }

    pub(super) fn validate_initial_agent_metadata(
        &self,
        metadata: &[tau_proto::AgentInitialMetadata],
    ) -> Result<(), String> {
        for item in metadata {
            self.validate_agent_metadata_key(&item.key)?;
            let value_bytes = tau_proto::encode_message_to_vec(&item.value)
                .map_err(|error| format!("failed to measure agent metadata value: {error}"))?;
            if tau_proto::MAX_AGENT_METADATA_VALUE_BYTES < value_bytes.len() {
                return Err("agent metadata value exceeds 64 KiB".to_owned());
            }
        }
        Ok(())
    }

    pub(super) fn validate_agent_metadata_unset(
        &mut self,
        unset: &tau_proto::AgentMetadataUnset,
    ) -> Result<(), String> {
        self.validate_agent_metadata_target(&unset.agent_id)?;
        self.validate_agent_metadata_key(&unset.key)
    }

    /// Validate canonical metadata replacements before commit.
    ///
    /// Request replacements deliberately bypass this validation and run the
    /// full metadata policy only after their request commit.
    pub(super) fn validate_agent_metadata_interceptor_replacement(
        &mut self,
        event: &Event,
    ) -> Result<(), String> {
        match event {
            Event::AgentMetadataSet(set) => self.validate_agent_metadata_set(set),
            Event::AgentMetadataUnset(unset) => self.validate_agent_metadata_unset(unset),
            _ => Ok(()),
        }
    }

    pub(super) fn resolve_start_agent_parent_cid(
        &self,
        query: &tau_proto::StartAgentRequest,
    ) -> Result<Option<AgentId>, String> {
        let explicit = query
            .parent_agent
            .as_ref()
            .map(|agent_id| {
                self.agent_runtime
                    .agent_registry
                    .agent_routes
                    .get(agent_id.as_str())
                    .cloned()
                    .ok_or_else(|| {
                        format!("parent_agent `{agent_id}` is not loaded in the current session")
                    })
            })
            .transpose()?;
        let tool_parent = query
            .tool_call_id
            .as_ref()
            .and_then(|call_id| self.tool_routing.tool_runtime.tool_agents.get(call_id))
            .cloned();
        if let (Some(explicit), Some(tool_parent)) = (&explicit, &tool_parent)
            && explicit != tool_parent
        {
            return Err("parent_agent does not match tool_call_id owner".to_owned());
        }
        Ok(explicit.or(tool_parent))
    }

    pub(super) fn parent_agent_id_for_cid(&self, cid: &AgentId) -> Option<tau_proto::AgentId> {
        self.agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.parent_agent_id.as_ref())
            .and_then(|parent_cid| self.agent_runtime.agent_registry.agents.get(parent_cid))
            .and_then(|parent| parent.identity.agent_id.clone())
    }

    pub(super) fn inherited_metadata_for_cid(
        &mut self,
        cid: &AgentId,
    ) -> Vec<(tau_proto::AgentMetadataKey, tau_core::AgentMetadataEntry)> {
        let Some(parent_agent_id) = self.parent_agent_id_for_cid(cid) else {
            return Vec::new();
        };
        match self
            .session_runtime
            .agent_store
            .load_agent(parent_agent_id.as_str())
        {
            Ok(Some(tree)) => tree.inheritable_metadata().into_iter().collect(),
            Ok(None) => Vec::new(),
            Err(error) => {
                self.emit_info(&format!(
                    "failed to load parent agent `{parent_agent_id}` metadata: {error}"
                ));
                Vec::new()
            }
        }
    }

    pub(super) fn prepare_start_agent_request(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        query: tau_proto::StartAgentRequest,
    ) -> Result<Option<PendingStartAgentRequest>, String> {
        if source_id != harness_connection_id() && !query.trusted_internal_spans.is_empty() {
            return Err(
                "configured extensions cannot assert trusted internal instruction spans".to_owned(),
            );
        }
        let extension_name = self
            .authenticated_source_name(source_id)
            .ok_or_else(|| "the requesting extension connection is unavailable".to_owned())?;
        let role = self.resolve_start_agent_request_role(&query)?;
        let duplicate_active =
            self.agent_runtime
                .agent_registry
                .agents
                .iter()
                .find_map(|(cid, conv)| {
                    let matches_query = conv.identity.source_connection.is_some()
                        && matches!(
                            &conv.identity.originator,
                            tau_proto::PromptOriginator::Extension { name, query_id }
                                if *name.as_str() == *extension_name && query_id == &query.query_id
                        );
                    matches_query.then(|| {
                        conv.identity
                            .agent_id
                            .clone()
                            .map(|agent_id| (cid.clone(), agent_id))
                    })?
                });
        if let Some((cid, agent_id)) = duplicate_active {
            if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                conv.identity.source_connection = Some(source_id.clone());
            }
            let Some(start_id) = self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .and_then(|agent| agent.identity.start_operation_id)
            else {
                self.fail_start_agent_request(
                    source_id,
                    query.query_id,
                    "existing agent has no live startup correlation".to_owned(),
                );
                return Ok(None);
            };
            if let Some(operation) = self
                .agent_runtime
                .agent_registry
                .start_coordinator
                .operations
                .get_mut(&start_id)
            {
                operation.source_id = source_id.clone();
            }
            self.accept_duplicate_start_agent_request(
                source_id,
                &query.query_id,
                &agent_id,
                start_id,
            );
            self.emit_info(&format!(
                "rebound duplicate start-agent-request `{}` from `{}` to existing agent `{}`",
                query.query_id, extension_name, agent_id
            ));
            return Ok(None);
        }
        if let Some(start_id) = self
            .agent_runtime
            .agent_registry
            .start_coordinator
            .requests
            .get(&(extension_name.clone(), query.query_id.clone()))
            .copied()
        {
            let accepted = {
                let operation = self
                    .agent_runtime
                    .agent_registry
                    .start_coordinator
                    .operations
                    .get_mut(&start_id)
                    .expect("start request index points to operation");
                operation.source_id = source_id.clone();
                operation.pending.source_id = source_id.clone();
                operation.accepted.clone()
            };
            if let Some(accepted) = accepted {
                let _ = self.runtime_io.bus.send_to(
                    source_id,
                    None,
                    HarnessOutputMessage::deliver(Event::StartAgentAccepted(accepted)),
                );
            }
            return Ok(None);
        }
        if let Some(idx) = self
            .agent_runtime
            .agent_registry
            .pending_start_requests
            .iter()
            .position(|pending| {
                *pending.extension_name == *extension_name
                    && pending.query.query_id == query.query_id
            })
        {
            let agent_id = self.agent_runtime.agent_registry.pending_start_requests[idx]
                .agent_id
                .clone();
            self.agent_runtime.agent_registry.pending_start_requests[idx].source_id =
                source_id.to_owned();
            self.emit_info(&format!(
                "rebound duplicate start-agent-request `{}` from `{}` to pending agent `{}`",
                query.query_id, extension_name, agent_id
            ));
            return Ok(None);
        }
        let agent_id = self.mint_available_agent_id_for_role(&role);
        let cid: AgentId = crate::parse_agent_id(&agent_id);

        // Resolve the parent agent at enqueue time for metadata inheritance:
        // tool-backed requests derive their parent from the conversation that
        // owns the triggering tool call; non-tool requests use an explicit
        // `parent_agent` when provided; otherwise they start with no parent.
        let parent_cid = self.resolve_start_agent_parent_cid(&query)?;
        let persistence = parent_cid
            .as_ref()
            .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
            .map(|agent| agent.identity.persistence)
            .unwrap_or_default();
        let pending = PendingStartAgentRequest {
            source_id: source_id.to_owned(),
            extension_name,
            query,
            role,
            cid,
            parent_cid,
            agent_id,
            persistence,
            pending_agent_message_wakes: VecDeque::new(),
        };
        if pending.query.query_id.len() > MAX_START_QUERY_ID_BYTES {
            return Err(format!(
                "start query id exceeds the {MAX_START_QUERY_ID_BYTES}-byte limit"
            ));
        }
        let retained_bytes = StartCoordinator::retained_payload_bytes(&pending)?;
        if !self
            .agent_runtime
            .agent_registry
            .start_coordinator
            .can_insert(retained_bytes)
        {
            return Err("too many or too much retained side-agent startup work".to_owned());
        }
        Ok(Some(pending))
    }

    /// Dispatch queued `StartAgentRequest`s in FIFO order. Directory/update
    /// coordination is owned by extensions such as `tau-ext-shell`, not by the
    /// harness.
    pub(crate) fn drain_pending_start_agent_requests(&mut self) -> Result<(), HarnessError> {
        loop {
            let Some(idx) = self.next_dispatchable_start_agent_request_index() else {
                return Ok(());
            };
            let pending = self
                .agent_runtime
                .agent_registry
                .pending_start_requests
                .remove(idx)
                .expect("index just located");
            self.begin_start_operation(pending);
        }
    }

    pub(super) fn next_dispatchable_start_agent_request_index(&self) -> Option<usize> {
        (!self
            .agent_runtime
            .agent_registry
            .pending_start_requests
            .is_empty())
        .then_some(0)
    }

    /// Compatibility hook for older teardown paths. Start-agent dispatch no
    /// longer holds harness-side update/exclusive locks, so release only tries
    /// to drain any queued requests left by earlier errors.
    pub(super) fn release_start_agent_request(&mut self, _cid: &AgentId) {
        if !self
            .agent_runtime
            .agent_registry
            .pending_start_requests
            .is_empty()
            && let Err(error) = self.drain_pending_start_agent_requests()
        {
            self.emit_harness_failure(&format!("queued start-agent dispatch failed: {error}"));
        }
    }

    /// Construct an ordinary side-agent endpoint without inventing an initial
    /// user instruction. The committed peer receive supplies its first input.
    pub(super) fn start_peer_agent_request(
        &mut self,
        pending: PendingStartAgentRequest,
    ) -> Result<(), HarnessError> {
        self.start_agent_request_inner(pending, false, true, false, None)
    }

    pub(super) fn start_agent_request_inner(
        &mut self,
        pending: PendingStartAgentRequest,
        publish_initial_instruction: bool,
        peer_entrypoint_endpoint: bool,
        creation_already_committed: bool,
        start_operation_id: Option<tau_proto::StartOperationId>,
    ) -> Result<(), HarnessError> {
        let PendingStartAgentRequest {
            source_id,
            extension_name,
            query,
            role,
            cid,
            parent_cid,
            agent_id,
            persistence,
            pending_agent_message_wakes,
        } = pending;
        let agent_id_proto = crate::parse_agent_id(&agent_id);
        let parent_call_id = query.tool_call_id.clone();
        let is_tool_backed = parent_call_id.is_some();
        let task_name = query.task_name.clone();
        let display_name = if is_tool_backed {
            normalize_display_name(task_name.as_deref())
                .or_else(|| self.display_name_for_new_agent(&agent_id, &role, task_name.as_deref()))
        } else {
            self.display_name_for_new_agent(&agent_id, &role, task_name.as_deref())
        };
        let conversation_role = if query.tool_call_id.is_some() || query.role.is_some() {
            Some(role)
        } else {
            None
        };
        let parent_agent_id = parent_cid.as_ref().and_then(|parent_cid| {
            self.agent_runtime
                .agent_registry
                .agents
                .contains_key(parent_cid)
                .then(|| parent_cid.clone())
        });
        let session_id = parent_agent_id
            .as_ref()
            .and_then(|parent_cid| self.agent_runtime.agent_registry.agents.get(parent_cid))
            .map(|parent| parent.identity.session_id.clone())
            .unwrap_or_else(|| self.session_runtime.current_session_id.clone());
        let creator = if query.tool_call_id.is_some() {
            let parent = parent_cid
                .as_ref()
                .and_then(|parent_cid| self.agent_runtime.agent_registry.agents.get(parent_cid))
                .ok_or_else(|| {
                    HarnessError::Participant(
                        "tool-backed agent creation lost its authenticated parent".to_owned(),
                    )
                })?;
            tau_proto::AgentCreator::Agent {
                session_id: parent.identity.session_id.clone(),
                agent_id: parent.identity.agent_id.clone().ok_or_else(|| {
                    HarnessError::Participant(
                        "tool-backed agent creation parent lacks durable identity".to_owned(),
                    )
                })?,
            }
        } else {
            let (name, instance_id) = self.extension_action_owner(&source_id);
            tau_proto::AgentCreator::Extension { name, instance_id }
        };
        // Start-agent requests create distinct agent transcripts, so their
        // runtime cursor starts at the root. Parent branch NodeIds belong
        // to the parent's agent log and must not be reused in the child log.
        let initial_head = None;
        if !creation_already_committed
            && persistence.is_ephemeral()
            && let Err(error) = self
                .session_runtime
                .agent_store
                .mark_agent_ephemeral(&agent_id)
        {
            return Err(HarnessError::Participant(format!(
                "failed to mark child agent `{agent_id}` ephemeral: {error}"
            )));
        }

        let originator = tau_proto::PromptOriginator::Extension {
            name: extension_name.clone(),
            query_id: query.query_id.clone(),
        };
        if is_tool_backed && &source_id == harness_connection_id() {
            self.agent_runtime
                .agent_registry
                .pending_builtin_delegates
                .insert(query.query_id.clone(), agent_id_proto.clone());
        }
        let initial_metadata: Vec<_> = peer_entrypoint_endpoint
            .then(|| tau_proto::AgentInitialMetadata {
                key: tau_proto::AgentMetadataKey::new(
                    path_crate_harness::subagents_tool::PEER_ENTRYPOINT_AGENT_METADATA_KEY,
                ),
                value: CborValue::Bool(true),
                inheritable: false,
            })
            .into_iter()
            .collect();
        let started = Event::AgentStarted(tau_proto::AgentStarted {
            creator: Some(creator),

            agent_id: agent_id_proto.clone(),
            parent_agent: parent_agent_id
                .as_ref()
                .and_then(|parent| self.target_agent_id_for_agent(parent)),
            role: conversation_role
                .clone()
                .unwrap_or_else(|| self.config.selected_role.clone()),
            display_name: display_name.clone(),
            metadata: initial_metadata.clone(),
            ephemeral: self.session_runtime.storage_mode.is_memory_only()
                || persistence.is_ephemeral(),
        });
        if !creation_already_committed {
            self.append_direct_agent_semantic_event(
                &agent_id,
                tau_core::AgentEventParent::InheritHead,
                started.clone(),
            )?;
        }
        let runtime_incarnation = self.mint_agent_runtime_incarnation();
        let mut conv = Agent::new(
            cid.clone(),
            runtime_incarnation,
            session_id.clone(),
            originator,
            initial_head,
            Some(source_id.clone()),
        );
        // Record parent request state and task metadata for teardown/background
        // ownership and child display metadata. Explicit-parent typed starts have
        // no tool call, but still retain their completed child after returning the
        // request result.
        conv.identity.parent_tool_call_id = parent_call_id;
        conv.identity.parent_agent_id = parent_agent_id;
        conv.identity.display_name = display_name;
        conv.identity.task_name = task_name;
        conv.identity.delegate_input_stats = query.input_stats;
        conv.identity.role = conversation_role;
        conv.identity.agent_id = Some(agent_id_proto.clone());
        conv.identity.persistence = persistence;
        conv.identity.start_operation_id = start_operation_id;
        conv.identity.peer_entrypoint_endpoint = peer_entrypoint_endpoint;
        conv.dispatch.pending_message_wakes = pending_agent_message_wakes;
        if let Some(last_node_id) = conv
            .dispatch
            .pending_message_wakes
            .iter()
            .filter_map(|wake| wake.node_id)
            .next_back()
        {
            conv.identity.head = Some(last_node_id);
        }
        self.agent_runtime
            .agent_registry
            .agent_routes
            .insert(agent_id_proto.clone(), cid.clone());
        self.agent_runtime
            .agent_registry
            .agents
            .insert(cid.clone(), conv);
        let buffered_activations = self
            .agent_runtime.agent_registry.agents
            .get(&cid)
            .into_iter()
            .flat_map(|agent| agent.dispatch.pending_message_wakes.iter())
            .filter_map(|wake| {
                wake.activation_observation.map(|observation| {
                    let kind = match wake.source {
                        path_crate_agent::PendingMessageWakeSource::AgentMessageReceived {
                            activation_class:
                                path_crate_agent::AgentMessageActivationClass::OrdinaryAgentInput,
                            ..
                        } => tau_proto::ActivationKind::AgentMessage,
                        path_crate_agent::PendingMessageWakeSource::AgentMessageReceived {
                            activation_class:
                                path_crate_agent::AgentMessageActivationClass::IsolatedWatchNotification,
                            ..
                        } => tau_proto::ActivationKind::WatchNotification,
                        path_crate_agent::PendingMessageWakeSource::MessageFact { .. } => {
                            tau_proto::ActivationKind::ExternalMessage
                        }
                    };
                    (observation, kind, wake.source_observation)
                })
            })
            .collect::<Vec<_>>();
        for (observation, kind, source_observation) in buffered_activations {
            self.append_activation_queued(&cid, observation, kind, source_observation, None);
        }
        if !creation_already_committed {
            self.agent_runtime
                .agent_registry
                .precommitted_starts
                .insert(agent_id.clone());
            self.enqueue_publish(
                None,
                started,
                true,
                true,
                Some(ConversationHeadSync {
                    cid: cid.clone(),
                    agent_id: Some(agent_id_proto.clone()),
                    session_generation: self.session_runtime.current_session_generation,
                    fold_parent: None,
                    suppress_activation_dispatch: false,
                    continuation: None,
                    notify_watchers: false,
                }),
            );
        }
        self.ensure_loaded_agent_for_agent_with_metadata(
            &cid,
            &agent_id_proto,
            initial_metadata,
            start_operation_id,
        );
        if let Some(display_name) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .and_then(|conv| normalize_display_name(conv.identity.display_name.as_deref()))
        {
            self.publish_for_agent(
                &cid,
                Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                    agent_id: agent_id_proto.clone(),
                    display_name,
                }),
            );
        }
        if peer_entrypoint_endpoint {
            self.write_loaded_agent_navigation_mode(
                &agent_id_proto,
                tau_proto::AgentNavigationMode::Active,
            )
            .map_err(|_| {
                HarnessError::Participant(format!(
                    "peer entrypoint agent `{agent_id}` lost its loaded navigation state"
                ))
            })?;
        } else {
            // Emit the initial generic agent stats snapshot as soon as the side
            // agent exists, before it spends tokens or starts nested tools.
            self.emit_agent_stats_updated(&cid);
        }

        if publish_initial_instruction {
            // Publish the accepted instruction into the side agent transcript and
            // dispatch only after that prompt folds into the agent head.
            let mut prompt = PendingPrompt::user(query.instruction);
            prompt.trusted_internal_spans = query.trusted_internal_spans;
            self.publish_pending_prompt_for_agent(&cid, prompt)?;
            self.drain_publish_idle_dispatches();
        }
        Ok(())
    }

    pub(super) fn detach_completed_parented_start_agent(&mut self, cid: &AgentId) {
        if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            // A completed parented worker remains addressable by its `agent_id`,
            // but it is no longer fulfilling the parent request or owned by the
            // extension query that started it. Clearing the transient side-query
            // fields makes later user prompts behave like a normal active
            // conversation on the same branch. The durable AgentStarted event
            // retains ancestry.
            conv.identity.originator = tau_proto::PromptOriginator::User;
            conv.identity.source_connection = None;
            conv.identity.parent_tool_call_id = None;
            conv.identity.parent_agent_id = None;
            conv.identity.restored_tool_backed_start = false;
            conv.identity.task_name = None;
            conv.identity.delegate_input_stats = Default::default();
        }
    }

    pub(super) fn agent_stats_snapshot(&self, cid: &AgentId) -> Option<AgentStatsUpdated> {
        let agent = self.agent_runtime.agent_registry.agents.get(cid)?;
        let agent_id = agent.identity.agent_id.as_ref()?;
        let stable_agent_id = agent_id.clone();
        let context_window = agent.execution.context_input_tokens.and_then(|_| {
            self.model_for_agent_role(agent).as_ref().and_then(|model| {
                context_window_for_model(&self.provider_runtime.model_info, model)
            })
        });
        Some(AgentStatsUpdated {
            session_id: self.session_runtime.current_session_id.clone(),
            agent_id: stable_agent_id.clone(),
            work_status: tau_proto::SessionAgentWorkStatus::new(
                agent.turn.work_status.phase(),
                agent.turn.work_status.title().map(ToOwned::to_owned),
            )
            .expect("harness work status is canonical"),
            navigation_mode: self
                .agent_runtime
                .agent_registry
                .navigation_modes
                .get(&stable_agent_id)
                .copied()
                .unwrap_or_else(|| {
                    tracing::error!(
                        target: "tau_harness",
                        agent_id = agent_id.as_str(),
                        "loaded agent is missing its navigation mode"
                    );
                    tau_proto::AgentNavigationMode::Active
                }),
            runtime_state: agent.turn.published_runtime_state,
            turn_activity: self.agent_turn_activity(&stable_agent_id, cid),
            tools: AgentToolStats {
                in_flight: agent.execution.tools_in_flight,
                started_total: agent.execution.tools_total,
            },
            context: AgentContextStats {
                input_tokens: agent
                    .execution
                    .context_input_tokens
                    .map(tau_proto::TokenCount::get),
                cached_tokens: agent
                    .execution
                    .context_cached_tokens
                    .map(tau_proto::TokenCount::get),
                context_window: context_window.map(tau_proto::TokenCount::get),
                percent_used: agent.execution.context_percent_used,
            },
            estimated_api_cost: self
                .agent_runtime
                .agent_registry
                .cost_ledger
                .self_cost(&stable_agent_id),
            creator_subtree_estimated_api_cost: self
                .agent_runtime
                .agent_registry
                .cost_ledger
                .creator_subtree_cost(&stable_agent_id),
        })
    }

    /// Reduce provider, active-call, and ambient state into one presentation
    /// value.
    pub(super) fn agent_turn_activity(
        &self,
        agent_id: &AgentId,
        cid: &AgentId,
    ) -> tau_proto::AgentTurnActivity {
        if self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|agent| agent.dispatch.in_flight_prompt.is_some())
        {
            return tau_proto::AgentTurnActivity::Responding;
        }
        let categories = self
            .tool_routing
            .tool_runtime
            .tool_turn
            .active_categories_for(cid);
        if categories.manipulator() {
            return tau_proto::AgentTurnActivity::Manipulating;
        }
        if categories.data_fetch() {
            return tau_proto::AgentTurnActivity::Fetching;
        }
        if categories.wait() {
            return tau_proto::AgentTurnActivity::Waiting;
        }
        if self
            .agent_runtime
            .agent_runtime_indicators
            .values()
            .any(|by_agent| {
                by_agent.get(agent_id).is_some_and(|indicators| {
                    indicators.contains(&tau_proto::AgentRuntimeIndicator::TimerScheduled)
                })
            })
        {
            return tau_proto::AgentTurnActivity::TimerScheduled;
        }
        tau_proto::AgentTurnActivity::Idle
    }

    /// Apply a committed complete ambient-indicator declaration from one
    /// source.
    pub(super) fn process_committed_agent_runtime_indicators(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        declaration: &tau_proto::AgentRuntimeIndicatorsDeclared,
    ) {
        let Some(extension) = peer_context
            .extension
            .as_ref()
            .filter(|extension| matches!(extension.kind, ClientKind::Tool | ClientKind::Core))
        else {
            return;
        };
        let source_id = &extension.source;
        let source_is_current = self.extensions.entries.get(source_id).is_some_and(|entry| {
            entry.connection_id == extension.source
                && entry.instance_id == extension.instance_id
                && entry.name == extension.publisher
                && matches!(entry.kind, ClientKind::Tool | ClientKind::Core)
                && entry.state != ExtensionState::Disconnected
        });
        let targets_current_live_agent = self
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(declaration.agent_id.as_str())
            .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
            .is_some_and(|agent| {
                !agent.dispatch.terminating
                    && agent.identity.session_id == self.session_runtime.current_session_id
                    && agent.identity.agent_id.as_deref() == Some(declaration.agent_id.as_str())
            });
        if !source_is_current || !targets_current_live_agent {
            return;
        }
        let unique = declaration
            .indicators
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        if MAX_AGENT_RUNTIME_INDICATORS < declaration.indicators.len()
            || unique.len() != declaration.indicators.len()
        {
            let message = format!(
                "agent runtime indicator declaration must contain at most {MAX_AGENT_RUNTIME_INDICATORS} unique values"
            );
            if let Err(error) = self.handle_extension_protocol_failure(source_id, message) {
                self.runtime_io
                    .publication
                    .pending_error
                    .get_or_insert(error);
            }
            return;
        }
        let before = self
            .agent_runtime
            .agent_runtime_indicators
            .values()
            .filter_map(|by_agent| by_agent.get(&declaration.agent_id))
            .fold(
                std::collections::BTreeSet::<tau_proto::AgentRuntimeIndicator>::new(),
                |mut all, values| {
                    all.extend(values);
                    all
                },
            );
        let by_agent = self
            .agent_runtime
            .agent_runtime_indicators
            .entry(source_id.clone())
            .or_default();
        if unique.is_empty() {
            by_agent.remove(&declaration.agent_id);
        } else {
            by_agent.insert(declaration.agent_id.clone(), unique);
        }
        if by_agent.is_empty() {
            self.agent_runtime
                .agent_runtime_indicators
                .remove(source_id);
        }
        let after = self
            .agent_runtime
            .agent_runtime_indicators
            .values()
            .filter_map(|by_agent| by_agent.get(&declaration.agent_id))
            .fold(BTreeSet::new(), |mut all, values| {
                all.extend(values);
                all
            });
        if before != after
            && let Some(cid) = self
                .agent_runtime
                .agent_registry
                .agent_routes
                .get(declaration.agent_id.as_str())
                .cloned()
        {
            self.emit_agent_stats_updated(&cid);
        }
    }

    /// Clear one disconnected source and refresh agents whose aggregate
    /// changed.
    pub(super) fn clear_agent_runtime_indicators_for_source(
        &mut self,
        source_id: &tau_proto::ConnectionId,
    ) {
        let Some(removed) = self
            .agent_runtime
            .agent_runtime_indicators
            .remove(source_id)
        else {
            return;
        };
        let affected = removed.keys().cloned().collect::<Vec<_>>();
        for agent_id in affected {
            if let Some(cid) = self
                .agent_runtime
                .agent_registry
                .agent_routes
                .get(agent_id.as_str())
                .cloned()
            {
                self.emit_agent_stats_updated(&cid);
            }
        }
    }

    /// Remove every source contribution for one unloaded agent.
    pub(super) fn clear_agent_runtime_indicators_for_agent(&mut self, agent_id: &AgentId) {
        for by_agent in self.agent_runtime.agent_runtime_indicators.values_mut() {
            by_agent.remove(agent_id);
        }
        self.agent_runtime
            .agent_runtime_indicators
            .retain(|_, by_agent| !by_agent.is_empty());
    }

    pub(super) fn emit_agent_stats_updated(&mut self, cid: &AgentId) {
        self.emit_agent_stats_updated_from(cid, None);
    }

    pub(super) fn emit_agent_stats_updated_from(
        &mut self,
        cid: &AgentId,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        if let Some(stats) = self.agent_stats_snapshot(cid) {
            self.publish_event(source, Event::AgentStatsUpdated(stats));
        }
    }

    /// Applies one absolute harness-owned navigation-mode write to a loaded
    /// agent.
    ///
    /// Every successful write publishes a fresh complete stats snapshot,
    /// including same-value writes. The caller remains responsible for
    /// authenticating the request and publishing any requester-directed
    /// outcome.
    pub(super) fn write_loaded_agent_navigation_mode(
        &mut self,
        agent_id: &tau_proto::AgentId,
        mode: tau_proto::AgentNavigationMode,
    ) -> Result<(), tau_proto::UiSetAgentNavigationModeRejection> {
        if !self
            .agent_runtime
            .agent_registry
            .session_loaded
            .contains(agent_id)
        {
            return Err(tau_proto::UiSetAgentNavigationModeRejection::AgentNotLoaded);
        }
        let Some(current_mode) = self
            .agent_runtime
            .agent_registry
            .navigation_modes
            .get_mut(agent_id)
        else {
            return Err(tau_proto::UiSetAgentNavigationModeRejection::AgentNotLoaded);
        };
        *current_mode = mode;
        if let Some(cid) = self
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(agent_id.as_str())
            .cloned()
        {
            self.emit_agent_stats_updated(&cid);
        }
        Ok(())
    }

    pub(super) fn handle_set_agent_navigation_mode(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        request: tau_proto::UiSetAgentNavigationMode,
    ) {
        let mode = match request.action {
            tau_proto::UiAgentNavigationModeAction::SetActive => {
                tau_proto::AgentNavigationMode::Active
            }
            tau_proto::UiAgentNavigationModeAction::SetActiveAuto => {
                tau_proto::AgentNavigationMode::ActiveAuto
            }
            tau_proto::UiAgentNavigationModeAction::SetSuspended => {
                tau_proto::AgentNavigationMode::Suspended
            }
        };
        let write = if request.session_id != self.session_runtime.current_session_id {
            Err(tau_proto::UiSetAgentNavigationModeRejection::StaleSession)
        } else {
            self.write_loaded_agent_navigation_mode(&request.agent_id, mode)
        };
        let outcome = if let Err(reason) = write {
            tau_proto::UiSetAgentNavigationModeOutcome::Rejected { reason }
        } else {
            tau_proto::UiSetAgentNavigationModeOutcome::Applied
        };
        let _ = self.runtime_io.bus.send_to(
            client_id,
            None,
            HarnessOutputMessage::deliver(Event::UiSetAgentNavigationModeResult(
                tau_proto::UiSetAgentNavigationModeResult {
                    request_id: request.request_id,
                    session_id: request.session_id,
                    agent_id: request.agent_id,
                    outcome,
                },
            )),
        );
    }

    pub(super) fn set_agent_turn_state(&mut self, cid: &AgentId, state: AgentTurnState) {
        let new_state = agent_runtime_state_for_turn(&state);
        let mut changed_agent_id = None;
        let mut finish = None;
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.turn.turn_state = state;
            if agent.turn.published_runtime_state != new_state {
                changed_agent_id = agent.identity.agent_id.clone();
                agent.turn.published_runtime_state = new_state;
                if new_state == tau_proto::AgentRuntimeState::Running {
                    agent.turn.turn_generation = agent.turn.turn_generation.saturating_next();
                }
            }
            if new_state != tau_proto::AgentRuntimeState::Running
                && let path_crate_agent::OuterTurnRuntimeState::Active(outer_turn_id) =
                    &agent.turn.outer_turn
                && let Some(agent_id) = agent.identity.agent_id.clone()
            {
                let outer_turn_id = outer_turn_id.clone();
                agent.turn.outer_turn =
                    path_crate_agent::OuterTurnRuntimeState::FinishInFlight(outer_turn_id.clone());
                finish = Some(tau_proto::AgentOuterTurnFinished {
                    automatic_compaction_decision: agent
                        .turn
                        .automatic_compaction
                        .decision_id()
                        .cloned(),
                    agent_id,
                    session_id: agent.identity.session_id.clone(),
                    outer_turn_id,
                    disposition: tau_proto::AgentOuterTurnDisposition::Settled,
                });
            }
        }

        if let Some(finish) = finish {
            self.publish_for_agent(cid, Event::AgentOuterTurnFinished(finish));
        }
        let Some(agent_id) = changed_agent_id else {
            return;
        };
        if new_state == tau_proto::AgentRuntimeState::Running {
            self.ensure_outer_turn_started(cid);
        }
        if new_state == tau_proto::AgentRuntimeState::Running {
            self.agent_runtime
                .agent_watch
                .provider_status
                .remove(agent_id.as_str());
            for watcher_id in self.watchers_for_agent(agent_id.as_str()) {
                if let Some(subscription_id) = self
                    .agent_runtime
                    .agent_watch
                    .subscriptions
                    .get(&(watcher_id, agent_id.to_string()))
                {
                    self.agent_runtime
                        .agent_watch
                        .provider_deliveries
                        .remove(subscription_id);
                }
            }
        }
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::AgentState(tau_proto::AgentStateChanged {
                agent_id,
                state: new_state,
            }),
        );
        self.emit_agent_stats_updated(cid);
        if new_state == tau_proto::AgentRuntimeState::Idle
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid)
        {
            agent.turn.lifecycle_notification_only_turn = false;
        }
    }

    pub(super) fn remove_agent(&mut self, cid: &AgentId) {
        let active_start = self
            .agent_runtime
            .agent_registry
            .start_coordinator
            .agents
            .get(cid)
            .copied();
        if let Some(start_id) = active_start {
            let closing = self
                .agent_runtime
                .agent_registry
                .start_coordinator
                .operations
                .get(&start_id)
                .is_some_and(|operation| operation.phase == StartPhase::ClosingFailure);
            if !closing {
                self.begin_start_failure(start_id, tau_proto::AgentStartFailure::Canceled);
            }
            return;
        }
        let active_standalone_prompts = self
            .prompt_coordination
            .standalone_accounting
            .owners
            .iter()
            .filter_map(|(prompt_id, owner)| (&owner.cid == cid).then_some(prompt_id.clone()))
            .collect::<Vec<_>>();
        if !active_standalone_prompts.is_empty()
            || self.has_unsettled_standalone_accounting_for(cid)
        {
            self.prompt_coordination
                .standalone_accounting
                .pending_agent_removals
                .insert(cid.clone());
        }
        for prompt_id in &active_standalone_prompts {
            self.publish_final_unknown_standalone_accounting(prompt_id);
        }
        if self
            .prompt_coordination
            .standalone_accounting
            .pending_agent_removals
            .contains(cid)
        {
            return;
        }
        let marked_owner = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| {
                let tree = self
                    .session_runtime
                    .agent_store
                    .agent(agent.identity.agent_id.as_deref()?)?;
                let checkpoint = tree.unresolved_marked_inference_checkpoint()?;
                Some((
                    agent.identity.agent_id.clone()?,
                    checkpoint.agent_prompt_id.clone(),
                    agent.identity.session_id.clone(),
                    agent.identity.originator.clone(),
                    agent.dispatch.terminating,
                ))
            });
        if let Some((agent_id, prompt_id, _session_id, originator, already_terminating)) =
            marked_owner
        {
            if !already_terminating {
                if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                    agent.dispatch.terminating = true;
                }
                self.publish_event_for_agent(
                    cid,
                    None,
                    Event::AgentPromptTerminated(AgentPromptTerminated {
                        automatic_compaction_decision: None,
                        agent_id,
                        agent_prompt_id: prompt_id,
                        reason: AgentPromptTerminationReason::Canceled,
                        originator,
                    }),
                );
            }
            return;
        }
        self.remove_agent_after_prompt_closure(cid);
    }

    /// Remove an endpoint as part of a normal completion or explicit cleanup.
    pub(super) fn remove_agent_expected(&mut self, cid: &AgentId) {
        if let Some(agent_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.clone())
            && !self
                .agent_runtime
                .agent_watch
                .pending_unload_reasons
                .contains_key(agent_id.as_str())
        {
            self.agent_runtime
                .agent_watch
                .expected_unloads
                .insert(agent_id.to_string());
        }
        self.remove_agent(cid);
    }

    /// Tear down runtime state only after any marked prompt closure committed.
    pub(super) fn remove_agent_after_prompt_closure(&mut self, cid: &AgentId) {
        self.tombstone_ephemeral_provider_prompts_for_agent(cid);
        self.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .remove(cid);
        self.reject_pending_ui_compaction(cid);
        self.runtime_io
            .publication
            .idle_dispatches
            .retain(|dispatch| &dispatch.cid != cid);
        let mut peer_internal_calls = self
            .tool_routing
            .tool_runtime
            .peer_internal_tool_agents
            .iter()
            .filter_map(|(call_id, owner)| (owner == cid).then_some(call_id.clone()))
            .collect::<Vec<_>>();
        peer_internal_calls.sort();
        for call_id in peer_internal_calls {
            let Some(tool) = self
                .tool_routing
                .tool_runtime
                .pending_tools
                .get(&call_id)
                .cloned()
            else {
                self.clear_tool_call_tracking(call_id.as_str());
                continue;
            };
            self.finish_prebuilt_internal_tool_error(ToolError {
                presentation: Default::default(),
                call_id,
                tool_name: tool.name,
                tool_type: tool.tool_type,
                message: "agent unloaded while peer-requested internal tool was pending".to_owned(),
                details: None,
                display: None,
                originator: PromptOriginator::User,
            });
        }
        self.retire_background_work_before_agent_unload(cid);
        let unloading_agent_id = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.clone());
        if let Some(unloading_agent_id) = unloading_agent_id {
            let unloading_agent_id_proto = unloading_agent_id.clone();
            self.clear_agent_runtime_indicators_for_agent(&unloading_agent_id_proto);
            self.prompt_coordination
                .context_discovery
                .pending_rendered_prompts
                .remove(&unloading_agent_id_proto);
            self.prompt_coordination
                .compaction_runtime
                .enqueued_inference_checkpoints
                .retain(|(agent_id, _)| agent_id != &unloading_agent_id_proto);
            self.peer_messaging
                .peer_input_rate
                .remove(&unloading_agent_id_proto);
            self.peer_messaging
                .uncommitted_peer_auto_starts
                .remove(&unloading_agent_id_proto);
            let staged_request_keys = self
                .prompt_coordination
                .compaction_runtime
                .pending_manual_acceptances
                .iter()
                .filter(|(_, pending)| {
                    matches!(
                        pending,
                        PendingManualCompactionAcceptance::ModelTool(staged)
                            if staged.request.tool_source().is_some_and(|source| {
                                source.caller_agent_id.as_str() == unloading_agent_id.as_str()
                            }) || staged.request.target_agent_id.as_str() == unloading_agent_id.as_str()
                    )
                })
                .map(|(request_key, _)| request_key.clone())
                .collect::<Vec<_>>();
            self.cancel_staged_model_acceptance_publications(&staged_request_keys);
            for request_key in staged_request_keys {
                let Some(staged) = self
                    .prompt_coordination
                    .compaction_runtime
                    .remove_pending_model_acceptance(&request_key)
                else {
                    continue;
                };
                let source = staged.request.required_tool_source();
                if source.caller_agent_id.as_str() != unloading_agent_id.as_str()
                    && let Some(caller_cid) = self
                        .runtime_agent_id_for_target_agent(Some(source.caller_agent_id.as_str()))
                {
                    self.finish_harness_owned_tool_with_error(
                        &caller_cid,
                        source.initiating_tool_call_id.clone(),
                        staged.visible_tool_name,
                        tau_proto::ToolType::Function,
                        "target_unavailable_or_unauthorized".to_owned(),
                        None,
                    );
                }
            }
            let requests: Vec<_> = self
                .prompt_coordination
                .compaction_runtime
                .accepted_manual_tools
                .values()
                .filter(|accepted| {
                    accepted.request.tool_source().is_some_and(|source| {
                        source.caller_agent_id.as_str() == unloading_agent_id.as_str()
                    }) || accepted.request.target_agent_id.as_str() == unloading_agent_id.as_str()
                })
                .map(|accepted| accepted.request.clone())
                .collect();
            for request in requests {
                if let Some(target_cid) =
                    self.runtime_agent_id_for_target_agent(Some(request.target_agent_id.as_str()))
                {
                    self.fail_accepted_manual_compaction(
                        &target_cid,
                        &request,
                        if request.target_agent_id.as_str() == unloading_agent_id.as_str() {
                            tau_proto::ManualCompactionRequestFailureReason::TargetUnloaded
                        } else {
                            tau_proto::ManualCompactionRequestFailureReason::Cancelled
                        },
                    );
                }
            }
        }
        self.fail_pending_initial_prompts(
            cid,
            tau_proto::AgentPromptFailureStage::LifecycleTeardown,
            "agent teardown discarded initial prompt",
        );
        let Some((session_id, agent_id)) = self
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(cid)
            .and_then(|conv| {
                conv.dispatch.terminating = true;
                conv.dispatch.pending_prompts.clear();
                conv.dispatch.pending_message_wakes.clear();
                conv.dispatch.activation_dispatch = path_crate_agent::ActivationDispatchState::None;
                conv.identity
                    .agent_id
                    .clone()
                    .map(|agent_id| (conv.identity.session_id.clone(), agent_id))
            })
        else {
            return;
        };
        self.cancel_agent_synchronized_publications(cid);
        if !self.unload_agent_from_session_if_loaded(&session_id, &agent_id) {
            let reason = self
                .agent_runtime
                .agent_watch
                .pending_unload_reasons
                .remove(agent_id.as_str())
                .or_else(|| {
                    (!self
                        .agent_runtime
                        .agent_watch
                        .expected_unloads
                        .remove(agent_id.as_str()))
                    .then_some(tau_proto::AgentWatchLifecycleReason::UnexpectedUnload)
                });
            self.retire_agent_watch_endpoint(&agent_id, reason);
            self.agent_runtime
                .agent_registry
                .agent_routes
                .remove(&agent_id);
            self.agent_runtime
                .agent_registry
                .stopped_ids
                .insert(agent_id);
            self.runtime_io
                .publication
                .capacity_rejected_activations
                .remove(cid);
            self.agent_runtime.agent_registry.agents.remove(cid);
        }
    }

    pub(super) fn unload_agent_from_session_if_loaded(
        &mut self,
        session_id: &SessionId,
        agent_id: &str,
    ) -> bool {
        self.clear_cache_refreshes(tau_proto::ProviderCacheRefreshCancelReason::AgentUnloaded);
        if session_id != &self.session_runtime.current_session_id {
            return false;
        }
        let agent_id_proto: tau_proto::AgentId = crate::parse_agent_id(agent_id);
        let already_loaded = self
            .agent_runtime
            .agent_registry
            .session_loaded
            .contains(&agent_id_proto)
            || match self.session_runtime.store.load_session(session_id.as_str()) {
                Ok(Some(membership)) => membership.contains_agent(&agent_id_proto),
                Ok(None) => false,
                Err(error) => {
                    self.emit_harness_failure(&format!(
                        "failed to load session while unloading agent `{agent_id}`: {error}"
                    ));
                    false
                }
            };
        if already_loaded {
            self.publish_event(
                None,
                Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                    session_id: session_id.clone(),
                    agent_id: agent_id_proto.clone(),
                }),
            );
            true
        } else {
            false
        }
    }

    pub(crate) fn role_group_name_for_role(&self, role: &str) -> String {
        self.config
            .available_role_groups
            .iter()
            .find(|group| group.roles.iter().any(|group_role| group_role == role))
            .map(|group| group.name.clone())
            .unwrap_or_else(|| role.to_owned())
    }

    /// Return public id and creation role for pending start requests without
    /// exposing their parent, prompt, source, or tool ownership.
    pub(crate) fn pending_agent_summary_data(&self) -> Vec<(String, String)> {
        self.agent_runtime
            .agent_registry
            .pending_start_requests
            .iter()
            .map(|pending| (pending.agent_id.clone(), pending.role.clone()))
            .chain(
                self.agent_runtime
                    .agent_registry
                    .start_coordinator
                    .operations
                    .values()
                    .filter(|operation| {
                        operation.phase != StartPhase::AwaitAcceptedCommit
                            && !self
                                .agent_runtime
                                .agent_registry
                                .agents
                                .contains_key(&operation.pending.cid)
                    })
                    .map(|operation| {
                        (
                            operation.pending.agent_id.clone(),
                            operation.pending.role.clone(),
                        )
                    }),
            )
            .collect()
    }

    pub(super) fn display_name_for_new_agent(
        &mut self,
        agent_id: &str,
        role: &str,
        task_name: Option<&str>,
    ) -> Option<String> {
        let fallback = normalize_display_name(task_name);
        let Some(template) = self.config.agent_display_name_template.clone() else {
            return fallback;
        };
        let role_group = self.role_group_name_for_role(role);
        match render_agent_template(
            &template,
            role,
            &role_group,
            agent_id,
            task_name,
            0,
            &mut self.agent_runtime.agent_registry.id_rng,
        ) {
            Ok(rendered) => normalize_display_name(Some(&rendered)).or(fallback),
            Err(error) => {
                self.emit_info(&format!(
                    "agent display name template failed to render: {error}; falling back to request display name"
                ));
                fallback
            }
        }
    }

    pub(crate) fn mint_available_agent_id_for_role(&mut self, role: &str) -> String {
        let template = self.config.agent_id_template.clone();
        let mut warnings = Vec::new();
        let role_group = self.role_group_name_for_role(role);
        let agent_routes = &self.agent_runtime.agent_registry.agent_routes;
        let stopped_agent_ids = &self.agent_runtime.agent_registry.stopped_ids;
        let agent_store = &self.session_runtime.agent_store;
        let pending_start_agent_requests =
            &self.agent_runtime.agent_registry.pending_start_requests;
        let reserved_start_agent_ids = &self.agent_runtime.agent_registry.start_coordinator.agents;
        let agent_id = mint_available_agent_id_for_role_with(
            role,
            &role_group,
            &template,
            |agent_id| {
                agent_routes.contains_key(agent_id)
                    || stopped_agent_ids.contains(agent_id)
                    || agent_store.agent_exists(agent_id)
                    || reserved_start_agent_ids.contains_key(agent_id)
                    || pending_start_agent_requests
                        .iter()
                        .any(|pending| pending.agent_id == agent_id)
            },
            &mut self.agent_runtime.agent_registry.id_rng,
            |kind, warning| warnings.push((kind, warning)),
        );
        for (kind, warning) in warnings {
            self.emit_agent_id_template_warning(kind, warning);
        }
        agent_id
    }

    pub(super) fn emit_agent_id_template_warning(
        &mut self,
        kind: AgentIdTemplateKind,
        warning: AgentIdMintWarning,
    ) {
        let source = match kind {
            AgentIdTemplateKind::Configured => "configured",
            AgentIdTemplateKind::Default => "default",
        };
        let message = match warning {
            AgentIdMintWarning::RenderFailed { error } => {
                format!(
                    "{source} agent id template failed to render: {error}; falling back to default template"
                )
            }
            AgentIdMintWarning::InvalidRendered { candidate, error } => format!(
                "{source} agent id template rendered invalid id `{candidate}`: {error}; falling back to default template"
            ),
            AgentIdMintWarning::CollisionsExceeded { attempts } => format!(
                "{source} agent id template failed to generate a unique id after {attempts} attempts; falling back to default template"
            ),
        };
        self.emit_info_important(&message);
    }

    #[cfg(test)]
    pub(crate) fn create_durable_user_agent(
        &mut self,
        session_id: SessionId,
        role: &str,
    ) -> AgentId {
        self.try_create_durable_user_agent(session_id, role)
            .expect("test agent creation")
    }

    pub(super) fn try_create_durable_user_agent(
        &mut self,
        session_id: SessionId,
        role: &str,
    ) -> Result<AgentId, HarnessError> {
        self.try_create_durable_user_agent_with_parent(session_id, role, None, Vec::new())
    }

    pub(super) fn try_create_durable_user_agent_with_parent(
        &mut self,
        session_id: SessionId,
        role: &str,
        parent_cid: Option<AgentId>,
        metadata: Vec<tau_proto::AgentInitialMetadata>,
    ) -> Result<AgentId, HarnessError> {
        self.try_create_user_agent_with_parent(
            session_id,
            role,
            parent_cid,
            metadata,
            tau_core::AgentPersistenceMode::Durable,
        )
    }

    pub(super) fn try_create_user_agent_with_parent(
        &mut self,
        session_id: SessionId,
        role: &str,
        parent_cid: Option<AgentId>,
        metadata: Vec<tau_proto::AgentInitialMetadata>,
        persistence: tau_core::AgentPersistenceMode,
    ) -> Result<AgentId, HarnessError> {
        let agent_id = self.mint_available_agent_id_for_role(role);
        if persistence.is_ephemeral() {
            self.session_runtime
                .agent_store
                .mark_agent_ephemeral(&agent_id)?;
        }
        let display_name = self.display_name_for_new_agent(&agent_id, role, None);
        let agent_id_proto = crate::parse_agent_id(&agent_id);
        let cid = agent_id_proto.clone();
        let started = Event::AgentStarted(tau_proto::AgentStarted {
            creator: Some(tau_proto::AgentCreator::default()),

            agent_id: agent_id_proto.clone(),
            parent_agent: parent_cid
                .as_ref()
                .and_then(|parent| self.agent_runtime.agent_registry.agents.get(parent))
                .and_then(|agent| agent.identity.agent_id.clone()),
            role: role.to_owned(),
            display_name: normalize_display_name(display_name.as_deref()),
            metadata: metadata.clone(),
            ephemeral: self.session_runtime.storage_mode.is_memory_only()
                || persistence.is_ephemeral(),
        });
        self.append_direct_agent_semantic_event(
            &agent_id,
            tau_core::AgentEventParent::InheritHead,
            started.clone(),
        )?;
        self.agent_runtime
            .agent_registry
            .session_ever_loaded
            .insert(agent_id_proto.clone());
        self.agent_runtime
            .agent_registry
            .precommitted_starts
            .insert(agent_id.clone());
        let runtime_incarnation = self.mint_agent_runtime_incarnation();
        let mut conv = Agent::new(
            cid.clone(),
            runtime_incarnation,
            session_id,
            tau_proto::PromptOriginator::User,
            None,
            None,
        );
        conv.identity.role = Some(role.to_owned());
        conv.identity.parent_agent_id = parent_cid;
        conv.identity.agent_id = Some(agent_id_proto.clone());
        conv.identity.display_name = display_name;
        conv.identity.persistence = persistence;
        self.agent_runtime
            .agent_registry
            .agents
            .insert(cid.clone(), conv);
        self.enqueue_publish(
            None,
            started,
            true,
            true,
            Some(ConversationHeadSync {
                cid: cid.clone(),
                agent_id: Some(agent_id_proto.clone()),
                session_generation: self.session_runtime.current_session_generation,
                fold_parent: None,
                suppress_activation_dispatch: false,
                continuation: None,
                notify_watchers: false,
            }),
        );
        self.publish_delegate_roles_context();
        self.ensure_loaded_agent_for_agent_with_metadata(&cid, &agent_id_proto, metadata, None);
        Ok(cid)
    }

    pub(crate) fn ensure_agent_id_for_agent(&mut self, cid: &AgentId) -> Option<AgentId> {
        if let Some(agent_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)?
            .identity
            .agent_id
            .clone()
        {
            self.ensure_loaded_agent_for_agent(cid, &agent_id);
            self.emit_agent_stats_updated(cid);
            return Some(agent_id);
        }
        let role = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map(|conv| self.role_name_for_agent(conv))?;
        let agent_id = self.mint_available_agent_id_for_role(&role);
        let agent_id_proto = crate::parse_agent_id(&agent_id);
        let display_name = self.display_name_for_new_agent(&agent_id, &role, None);
        let persistence = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map(|conv| conv.identity.persistence)
            .unwrap_or_default();
        if persistence.is_ephemeral()
            && let Err(error) = self
                .session_runtime
                .agent_store
                .mark_agent_ephemeral(&agent_id)
        {
            self.emit_harness_failure(&format!(
                "failed to reserve ephemeral agent `{agent_id}`: {error}"
            ));
            return None;
        }
        let started = Event::AgentStarted(tau_proto::AgentStarted {
            creator: Some(tau_proto::AgentCreator::default()),

            agent_id: agent_id_proto.clone(),
            parent_agent: self.parent_agent_id_for_cid(cid),
            role: role.clone(),
            display_name: normalize_display_name(display_name.as_deref()).or_else(|| {
                self.agent_runtime
                    .agent_registry
                    .agents
                    .get(cid)
                    .and_then(|agent| {
                        normalize_display_name(agent.identity.display_name.as_deref())
                    })
            }),
            metadata: Vec::new(),
            ephemeral: self.session_runtime.storage_mode.is_memory_only()
                || persistence.is_ephemeral(),
        });
        if let Err(error) = self.append_direct_agent_semantic_event(
            &agent_id,
            tau_core::AgentEventParent::InheritHead,
            started.clone(),
        ) {
            self.emit_harness_failure(&format!(
                "failed to commit creation for agent `{agent_id}`: {error}"
            ));
            return None;
        }
        self.agent_runtime
            .agent_registry
            .session_ever_loaded
            .insert(agent_id_proto.clone());
        self.agent_runtime
            .agent_registry
            .precommitted_starts
            .insert(agent_id.clone());
        if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            conv.identity.agent_id = Some(agent_id_proto.clone());
            if normalize_display_name(conv.identity.display_name.as_deref()).is_none() {
                conv.identity.display_name = display_name;
            }
        }
        self.publish_delegate_roles_context();
        self.enqueue_publish(
            None,
            started,
            true,
            true,
            Some(ConversationHeadSync {
                cid: cid.clone(),
                agent_id: Some(agent_id_proto.clone()),
                session_generation: self.session_runtime.current_session_generation,
                fold_parent: None,
                suppress_activation_dispatch: false,
                continuation: None,
                notify_watchers: false,
            }),
        );
        self.ensure_loaded_agent_for_agent(cid, &agent_id_proto);
        self.emit_agent_stats_updated(cid);
        Some(agent_id_proto)
    }

    pub(super) fn ensure_loaded_agent_for_agent(&mut self, cid: &AgentId, agent_id: &AgentId) {
        self.ensure_loaded_agent_for_agent_with_metadata(cid, agent_id, Vec::new(), None);
    }

    /// Returns whether an existing agent is being introduced to the current
    /// session for the first time.
    pub(super) fn existing_agent_is_new_to_current_session(
        &mut self,
        agent_id: &tau_proto::AgentId,
    ) -> bool {
        if self
            .agent_runtime
            .agent_registry
            .session_ever_loaded
            .contains(agent_id)
        {
            return false;
        }
        match self.session_runtime.agent_store.agent_events(agent_id) {
            Ok(events) => !events.is_empty(),
            Err(error) => {
                self.emit_harness_failure(&format!(
                    "failed to inspect agent `{agent_id}` history before loading: {error}"
                ));
                false
            }
        }
    }

    pub(super) fn ensure_loaded_agent_for_agent_with_metadata(
        &mut self,
        cid: &AgentId,
        agent_id: &AgentId,
        initial_metadata: Vec<tau_proto::AgentInitialMetadata>,
        start_operation_id: Option<tau_proto::StartOperationId>,
    ) {
        self.agent_runtime
            .agent_registry
            .stopped_ids
            .remove(agent_id);
        self.agent_runtime
            .agent_registry
            .restored_unavailable
            .remove(agent_id);
        let has_creation = self
            .session_runtime
            .agent_store
            .agent_has_committed_identity(agent_id);
        if !has_creation {
            let persistence = self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .map(|agent| agent.identity.persistence)
                .unwrap_or_default();
            let started =
                Event::AgentStarted(tau_proto::AgentStarted {
                    creator: Some(tau_proto::AgentCreator::default()),

                    agent_id: agent_id.clone(),
                    parent_agent: self.parent_agent_id_for_cid(cid),
                    role: self
                        .agent_runtime
                        .agent_registry
                        .agents
                        .get(cid)
                        .map(|agent| self.role_name_for_agent(agent))
                        .unwrap_or_else(|| self.config.selected_role.clone()),
                    display_name: self.agent_runtime.agent_registry.agents.get(cid).and_then(
                        |agent| normalize_display_name(agent.identity.display_name.as_deref()),
                    ),
                    metadata: initial_metadata.clone(),
                    ephemeral: self.session_runtime.storage_mode.is_memory_only()
                        || persistence.is_ephemeral(),
                });
            if let Err(error) = self.append_direct_agent_semantic_event(
                agent_id.as_str(),
                tau_core::AgentEventParent::InheritHead,
                started.clone(),
            ) {
                self.emit_harness_failure(&format!(
                    "failed to commit creation for agent `{agent_id}`: {error}"
                ));
                return;
            }
            self.agent_runtime
                .agent_registry
                .precommitted_starts
                .insert(agent_id.to_string());
            self.enqueue_publish(
                None,
                started,
                true,
                true,
                Some(ConversationHeadSync {
                    cid: cid.clone(),
                    agent_id: Some(agent_id.clone()),
                    session_generation: self.session_runtime.current_session_generation,
                    fold_parent: None,
                    suppress_activation_dispatch: false,
                    continuation: None,
                    notify_watchers: false,
                }),
            );
        } else {
            // Existing identities can become loaded after cold rehydration. Fold
            // their already-validated immutable creation fact now, rather than
            // losing creator cost propagation until another daemon resume.
            self.seed_agent_creator_topology(agent_id);
        }
        // New agents reached this point only after their creation record
        // committed; existing agents already have validated journal identity.
        self.agent_runtime
            .agent_registry
            .agent_routes
            .insert(agent_id.clone(), cid.clone());
        let default_navigation_mode = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map(|agent| default_navigation_mode(&agent.identity.originator))
            .unwrap_or_default();
        self.agent_runtime
            .agent_registry
            .navigation_modes
            .entry(agent_id.clone())
            .or_insert(default_navigation_mode);
        let role = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map(|conv| self.role_name_for_agent(conv));
        let persistence = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map(|conv| conv.identity.persistence)
            .unwrap_or_default();
        let prompt_index_initialized = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|agent| agent.dispatch.prompt_index_initialized);
        if !prompt_index_initialized {
            let next_prompt_index = match self.session_runtime.agent_store.load_agent(agent_id) {
                Ok(Some(_)) => self.next_prompt_index_from_log(agent_id),
                Ok(None) => 0,
                Err(error) => {
                    self.emit_harness_failure(&format!(
                        "failed to load agent `{agent_id}`: {error}"
                    ));
                    0
                }
            };
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                agent.dispatch.next_prompt_index = next_prompt_index;
                agent.dispatch.prompt_index_initialized = true;
            }
        }
        let already_loaded = self
            .agent_runtime
            .agent_registry
            .session_loaded
            .contains(agent_id)
            || match self
                .session_runtime
                .store
                .load_session(self.session_runtime.current_session_id.as_str())
            {
                Ok(Some(membership)) => membership.contains_agent(agent_id),
                Ok(None) => false,
                Err(error) => {
                    self.emit_harness_failure(&format!(
                        "failed to load session while ensuring agent `{agent_id}`: {error}"
                    ));
                    false
                }
            };
        if !already_loaded {
            let warn_about_changed_session =
                self.existing_agent_is_new_to_current_session(agent_id);
            if warn_about_changed_session {
                self.prompt_coordination
                    .pending_notices
                    .changed_session_agents
                    .insert((
                        self.session_runtime.current_session_id.clone(),
                        agent_id.clone(),
                    ));
            }
            self.agent_runtime
                .agent_registry
                .session_ever_loaded
                .insert(agent_id.clone());
            self.agent_runtime
                .agent_registry
                .session_loaded
                .insert(agent_id.clone());
            let _ = (role, initial_metadata);
            for (key, entry) in self.inherited_metadata_for_cid(cid) {
                self.enqueue_publish(
                    None,
                    Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                        agent_id: agent_id.clone(),
                        key,
                        value: entry.value,
                        mutation_id: None,
                        inheritable: entry.inheritable,
                    }),
                    true,
                    false,
                    Some(ConversationHeadSync {
                        cid: cid.clone(),
                        agent_id: Some(agent_id.clone()),
                        session_generation: self.session_runtime.current_session_generation,
                        fold_parent: None,
                        suppress_activation_dispatch: false,
                        continuation: None,
                        notify_watchers: false,
                    }),
                );
            }
        }
        if self
            .prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(agent_id)
            || self
                .prompt_coordination
                .context_discovery
                .frozen_agents
                .contains_key(agent_id)
        {
            return;
        }
        let agent_initialization_id = self.mint_agent_initialization_id();
        let waiting_on =
            self.agent_context_provider_ids(agent_id.clone(), agent_initialization_id.clone());
        self.prompt_coordination
            .context_discovery
            .pending_agents
            .insert(
                agent_id.clone(),
                PendingAgentDiscovery {
                    initialization_id: agent_initialization_id.clone(),
                    skill_candidates: self
                        .prompt_coordination
                        .context_discovery
                        .skill_candidates
                        .clone(),
                    skills: self.prompt_coordination.context_discovery.skills.clone(),
                    agents_files: self
                        .prompt_coordination
                        .context_discovery
                        .agents_files
                        .clone(),
                    waiting_on,
                },
            );
        let loaded = Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            session_id: self.session_runtime.current_session_id.clone(),
            agent_id: agent_id.clone(),
            agent_initialization_id,
            ephemeral: self.session_runtime.storage_mode.is_memory_only()
                || persistence.is_ephemeral(),
        });
        if let Some(start_id) = start_operation_id {
            self.enqueue_start_phase(
                loaded,
                !already_loaded,
                false,
                StartPhaseOwner {
                    start_id,
                    expected_phase: StartPhase::AwaitLoadedCommit,
                    expected_event: tau_proto::EventName::SESSION_AGENT_LOADED,
                },
            );
        } else if already_loaded {
            self.enqueue_publish(None, loaded, false, false, None);
        } else {
            self.publish_event(None, loaded);
        }
        if self.runtime_io.publication.pending_intercept.is_none()
            && self
                .prompt_coordination
                .context_discovery
                .pending_agents
                .get(agent_id)
                .is_some_and(|pending| pending.waiting_on.is_empty())
            && let Err(error) = self.finalize_agent_discovery(agent_id)
        {
            self.emit_harness_failure(&format!("failed to finalize agent discovery: {error}"));
        }
    }

    /// Finds the first collision-free numeric prompt sequence from durable ids.
    pub(super) fn next_prompt_index_from_log(&self, agent_id: &str) -> u64 {
        self.session_runtime
            .agent_store
            .agent_events(agent_id)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|record| match record.event {
                Event::AgentPromptStarted(prompt) => Some(prompt.agent_prompt_id.to_string()),
                Event::ProviderResponseFinished(response) => {
                    Some(response.agent_prompt_id.to_string())
                }
                Event::AgentInferenceDispatchStarted(checkpoint) => {
                    Some(checkpoint.agent_prompt_id.to_string())
                }
                Event::AgentStandaloneCompactionStarted(started) => {
                    Some(started.transaction_id.to_string())
                }
                _ => None,
            })
            .filter_map(|id| id.rsplit('-').next()?.parse::<u64>().ok())
            .max()
            .map_or(0, |index| index.saturating_add(1))
    }
}
