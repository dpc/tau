//! Owns prompt dispatch preparation and provider-visible materialization.
//!
//! This boundary preserves system-template gating and the committed compaction
//! window authority described by `SPEC-compaction-and-context-recovery`.

#[cfg(test)]
use super::prompt_materialization_timing::note_count_work;
use super::prompt_materialization_timing::{
    MaterializationCounts, MaterializationStage, PromptMaterializationTiming, stage_start,
};
use super::*;

#[cfg(test)]
thread_local! {
    static DISPATCH_PROVIDER_SORT_COUNT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
#[path = "prompt_materialization_web_tools_tests.rs"]
mod web_tools_tests;

/// Logical web operation selected independently at prompt materialization.
#[derive(Clone, Copy, Eq, PartialEq)]
enum LogicalWebOperation {
    /// Search for web sources.
    Search,
    /// Fetch one caller-selected page.
    Fetch,
}

impl LogicalWebOperation {
    /// Stable diagnostic name.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Fetch => "fetch",
        }
    }

    /// Required model-visible tool alias.
    const fn model_alias(self) -> &'static str {
        match self {
            Self::Search => "web_search",
            Self::Fetch => "web_fetch",
        }
    }

    /// Neutral operation metadata tag.
    const fn operation_tag(self) -> &'static str {
        match self {
            Self::Search => tau_proto::WEB_SEARCH_TOOL_TAG,
            Self::Fetch => tau_proto::WEB_FETCH_TOOL_TAG,
        }
    }

    /// Required domain-enforcement metadata tag.
    const fn enforcement_tag(self) -> &'static str {
        match self {
            Self::Search => tau_proto::WEB_PROVIDER_FILTER_DOMAIN_ENFORCEMENT_TAG,
            Self::Fetch => tau_proto::WEB_REQUESTED_TARGET_DOMAIN_ENFORCEMENT_TAG,
        }
    }
}

/// Result of compiling logical web policy against one exact route.
struct CompiledWebTools {
    /// Selected provider-hosted definitions.
    hosted_tools: Vec<tau_proto::HostedToolDefinition>,
    /// Selected ordinary internal tool names.
    retained_tools: HashSet<ToolName>,
    /// Hidden policy for selected ordinary tools.
    invocation_policies: HashMap<ToolName, tau_proto::ToolInvocationPolicy>,
}

fn suppress_declared_web_candidates(
    policy: &tau_config::WebToolsPolicy,
    specs: &mut Vec<tau_proto::ToolSpec>,
) {
    let declared_candidates = policy.declared_tool_names().collect::<HashSet<_>>();
    specs.retain(|spec| !declared_candidates.contains(&spec.name));
}

fn hosted_web_search_collides(
    hosted_tools: &[tau_proto::HostedToolDefinition],
    specs: &[tau_proto::ToolSpec],
) -> bool {
    !hosted_tools.is_empty()
        && specs.iter().any(|spec| {
            spec.model_visible_name
                .as_ref()
                .unwrap_or(&spec.name)
                .as_str()
                == "web_search"
        })
}

/// Fully compiled provider-visible prompt surface.
struct MaterializedPromptSurface {
    /// Authorized and selected ordinary tool metadata.
    tool_specs: Vec<tau_proto::ToolSpec>,
    /// Provider-facing ordinary tool definitions.
    tool_definitions: Vec<ToolDefinition>,
    /// Provider-facing hosted tool definitions.
    hosted_tools: Vec<tau_proto::HostedToolDefinition>,
    /// Hidden policies for ordinary invocations.
    invocation_policies: HashMap<ToolName, tau_proto::ToolInvocationPolicy>,
    /// Rendered system prompt based on the selected surface.
    system_prompt: String,
}

/// Reset the deterministic dispatch provider-sort work counter.
#[cfg(test)]
pub(super) fn reset_dispatch_provider_sort_count() {
    DISPATCH_PROVIDER_SORT_COUNT.set(0);
}

/// Return the dispatch provider-sort work count for the current test.
#[cfg(test)]
pub(super) fn dispatch_provider_sort_count() -> usize {
    DISPATCH_PROVIDER_SORT_COUNT.get()
}

/// Prompt-surface preparation failure that preserves duplicate-tool diagnostics
/// separately from template rendering failures.
#[derive(Debug)]
pub(super) enum PromptSurfaceError {
    /// Duplicate provider-visible tool name in the effective snapshot.
    DuplicateToolName(String),
    /// Strict Handlebars rendering failure.
    Render(handlebars::RenderError),
    /// Logical web policy required a capability unavailable on the exact route.
    WebUnavailable(String),
}

/// Count serialized JSON bytes without retaining schema content.
fn serialized_json_len(value: &serde_json::Value) -> usize {
    #[cfg(test)]
    note_count_work();
    struct ByteCounter(usize);

    impl std::io::Write for ByteCounter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, value).map_or(0, |()| counter.0)
}

/// Compile role policy against one exact route and one authorized tool
/// snapshot.
fn compile_web_tools(
    policy: &tau_config::WebToolsPolicy,
    model: &tau_proto::ProviderModelInfo,
    specs: &[tau_proto::ToolSpec],
) -> Result<CompiledWebTools, String> {
    let allowed_domains = policy.allowed_domains().map(<[String]>::to_vec);
    let domains_available = allowed_domains
        .as_ref()
        .is_none_or(|domains| !domains.is_empty());
    let native_capability = model
        .hosted_tool_capabilities
        .iter()
        .map(|capability| {
            let tau_proto::ProviderHostedToolCapability::WebSearch {
                access_modes,
                supports_allowed_domains,
                supports_context_size,
            } = capability;
            (
                access_modes.as_slice(),
                *supports_allowed_domains,
                *supports_context_size,
            )
        })
        .next();
    let mut retained = HashSet::new();
    let mut invocation_policies = HashMap::new();
    let mut hosted = Vec::new();

    for (operation, logical) in [
        (LogicalWebOperation::Search, policy.search()),
        (LogicalWebOperation::Fetch, policy.fetch()),
    ] {
        let mut candidates = logical.candidates().collect::<Vec<_>>();
        candidates.sort_by(|(left_name, left), (right_name, right)| {
            left.priority()
                .cmp(&right.priority())
                .then_with(|| left_name.cmp(right_name))
        });
        let winner = candidates.into_iter().find(|(_, candidate)| {
            if !candidate.enabled() || !domains_available {
                return false;
            }
            match candidate {
                tau_config::WebToolCandidate::ModelProvider {
                    access,
                    context_size,
                    ..
                } => {
                    operation == LogicalWebOperation::Search
                        && native_capability.is_some_and(|(access_modes, domains, context)| {
                            let requested_access = if *access == tau_config::WebSearchAccess::Live {
                                tau_proto::ProviderWebSearchAccess::Live
                            } else {
                                tau_proto::ProviderWebSearchAccess::Cached
                            };
                            access_modes.contains(&requested_access)
                                && (allowed_domains.is_none() || domains)
                                && (context_size.is_none() || context)
                        })
                }
                tau_config::WebToolCandidate::Tool { tool, .. } => specs.iter().any(|spec| {
                    spec.name == *tool
                        && spec.tool_type == tau_proto::ToolType::Function
                        && spec
                            .model_visible_name
                            .as_ref()
                            .unwrap_or(&spec.name)
                            .as_str()
                            == operation.model_alias()
                        && spec
                            .tags
                            .iter()
                            .any(|tag| tag.as_str() == operation.operation_tag())
                        && (allowed_domains.is_none()
                            || spec
                                .tags
                                .iter()
                                .any(|tag| tag.as_str() == operation.enforcement_tag()))
                }),
            }
        });
        match winner {
            Some((
                _,
                tau_config::WebToolCandidate::ModelProvider {
                    access,
                    context_size,
                    ..
                },
            )) => {
                hosted.push(tau_proto::HostedToolDefinition::WebSearch {
                    access: if *access == tau_config::WebSearchAccess::Live {
                        tau_proto::ProviderWebSearchAccess::Live
                    } else {
                        tau_proto::ProviderWebSearchAccess::Cached
                    },
                    context_size: *context_size,
                    allowed_domains: allowed_domains.clone().unwrap_or_default(),
                });
            }
            Some((_, tau_config::WebToolCandidate::Tool { tool, .. })) => {
                retained.insert(tool.clone());
                if allowed_domains.is_some() {
                    invocation_policies.insert(
                        tool.clone(),
                        tau_proto::ToolInvocationPolicy {
                            allowed_web_domains: allowed_domains.clone(),
                        },
                    );
                }
            }
            None if logical.unavailable() == tau_config::WebToolUnavailablePolicy::Error => {
                return Err(format!(
                    "logical web {} is unavailable for exact route `{}`",
                    operation.as_str(),
                    model.id,
                ));
            }
            None => {}
        }
    }
    Ok(CompiledWebTools {
        hosted_tools: hosted,
        retained_tools: retained,
        invocation_policies,
    })
}

impl Harness {
    /// Acquire one actually sorted provider snapshot and account its work in
    /// deterministic regression tests.
    fn sorted_prompt_tool_providers(&self) -> Vec<&tau_core::ToolProvider> {
        #[cfg(test)]
        DISPATCH_PROVIDER_SORT_COUNT.set(DISPATCH_PROVIDER_SORT_COUNT.get() + 1);
        self.tool_routing.registry.all_tool_providers()
    }

    /// Activates already-committed input for the first user agent in the
    /// requested session.
    ///
    /// Tests establish the intended semantic prompt fact before calling this
    /// helper. It drives the production publish-idle activation boundary
    /// without appending another transcript entry.
    #[cfg(test)]
    pub(super) fn send_prompt_to_agent(&mut self, session_id: &str) -> AgentPromptId {
        let cid = self
            .agent_runtime
            .agent_registry
            .agents
            .iter()
            .find(|(_, conv)| {
                conv.identity.session_id.as_str() == session_id
                    && conv.identity.originator.is_user()
            })
            .map(|(cid, _)| cid.clone())
            .expect("test requires an existing user agent");
        self.dispatch_activation_after_publish_idle(&cid);
        self.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .and_then(|agent| agent.dispatch.in_flight_prompt.clone())
            .expect("test prompt requires a selected model and durable dispatch owner")
    }

    /// Persist the sole ordinary outer-turn start once its durable inference
    /// checkpoint supplies both the initiating occurrence and unique prompt id.
    pub(super) fn ensure_outer_turn_started(&mut self, cid: &AgentId) {
        let activation = self.outer_turn_activation(cid);
        let restored_turn = activation.as_ref().and_then(|(_, prompt_id)| {
            let turn_id = tau_proto::AgentOuterTurnId::for_prompt(prompt_id);
            let agent = self.agent_runtime.agent_registry.agents.get(cid)?;
            self.session_runtime
                .agent_store
                .agent(agent.identity.agent_id.as_deref()?)
                .is_some_and(|tree| tree.outer_turn_is_open(&turn_id))
                .then_some(turn_id)
        });
        if let Some(turn_id) = restored_turn {
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                agent.turn.outer_turn = path_crate_agent::OuterTurnRuntimeState::Active(turn_id);
                agent.turn.terminal_status_was_available = false;
                agent.turn.terminal_notice_eligible = false;
                agent.turn.terminal_notice_outer_turn_id = None;
                agent.turn.terminal_context_size_alerts.clear();
            }
            return;
        }
        let runtime_id = self
            .agent_runtime
            .agent_registry
            .accounting_runtime_id
            .clone();
        let start = self
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(cid)
            .and_then(|agent| {
                let (activation, prompt_id) = activation?;
                if !matches!(
                    agent.turn.outer_turn,
                    path_crate_agent::OuterTurnRuntimeState::None
                ) {
                    return None;
                }
                let durable_agent_id = agent.identity.agent_id.clone()?;
                let outer_turn_id = tau_proto::AgentOuterTurnId::for_prompt(&prompt_id);
                agent.turn.outer_turn =
                    path_crate_agent::OuterTurnRuntimeState::Active(outer_turn_id.clone());
                agent.turn.terminal_status_was_available = false;
                agent.turn.terminal_notice_eligible = false;
                agent.turn.terminal_notice_outer_turn_id = None;
                agent.turn.terminal_context_size_alerts.clear();
                Some(tau_proto::AgentOuterTurnStarted {
                    agent_id: durable_agent_id,
                    session_id: agent.identity.session_id.clone(),
                    outer_turn_id,
                    agent_prompt_id: prompt_id,
                    runtime_id,
                    activation,
                })
            });
        if let Some(start) = start {
            self.publish_for_agent(cid, Event::AgentOuterTurnStarted(start));
        }
    }

    /// Resolve the first durable transcript occurrence after an inference
    /// checkpoint's activation cut.
    pub(super) fn outer_turn_activation(
        &self,
        cid: &AgentId,
    ) -> Option<(tau_proto::AgentOuterTurnActivation, AgentPromptId)> {
        let agent = self.agent_runtime.agent_registry.agents.get(cid)?;
        let (through, cut, prompt_id) = match &agent.dispatch.activation_dispatch {
            path_crate_agent::ActivationDispatchState::AwaitingCheckpoint {
                agent_prompt_id,
                through,
                dispatch,
                ..
            } if dispatch.operation == tau_proto::PromptOperation::Inference => {
                (*through, dispatch.activation_cut, agent_prompt_id.clone())
            }
            path_crate_agent::ActivationDispatchState::DispatchUncertain {
                agent_prompt_id,
                through,
                operation: Some(tau_proto::PromptOperation::Inference),
                activation_cut: Some(cut),
                ..
            } => (*through, *cut, agent_prompt_id.clone()),
            _ => return None,
        };
        let tree = self
            .session_runtime
            .agent_store
            .agent(agent.identity.agent_id.as_deref()?)?;
        let path = tree.branch_node_ids_from(match through {
            tau_proto::AgentHead::Root => None,
            tau_proto::AgentHead::Node(node) => Some(node),
        });
        let occurrence = match cut {
            tau_proto::AgentHead::Root => path.first().copied(),
            tau_proto::AgentHead::Node(cut) => path
                .iter()
                .position(|candidate| *candidate == cut)
                .and_then(|index| path.get(index.saturating_add(1)).copied()),
        };
        let activation = tau_proto::AgentOuterTurnActivation::Journal {
            occurrence: tau_proto::AgentHead::Node(occurrence?),
        };
        Some((activation, prompt_id))
    }

    /// Convert a notification-only running generation into an observable mixed
    /// turn by emitting its delayed start before any eventual stop.
    pub(super) fn promote_lifecycle_notification_turn(&mut self, cid: &AgentId) {
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            if !agent.turn.lifecycle_notification_only_turn
                || agent.turn.published_runtime_state != tau_proto::AgentRuntimeState::Running
            {
                return;
            }
            agent.turn.lifecycle_notification_only_turn = false;
        }
    }

    /// Mints a new `AgentPromptId`, registers it with `cid`'s conversation, and
    /// binds the full transient provider request to the durable compact
    /// `AgentPromptStarted` fact's post-commit continuation.
    ///
    /// Linear-prefix invariant: each subsequent prompt for the same
    /// agent branch must be a strict byte-prefix extension of the prior
    /// one. Provider prompt caches (OpenAI, Anthropic, etc.) key
    /// entirely off the prefix bytes, so any per-turn churn in
    /// `system_prompt`, `tools`, or earlier messages busts the cache.
    /// See `linear_agent_prompts_strictly_extend_previous_messages`.
    pub(crate) fn send_prompt_to_agent_for(&mut self, cid: &AgentId) -> Option<AgentPromptId> {
        self.send_prompt_to_agent_for_with_timing(cid, None)
    }

    /// Materialize one inference using timing started by the exact durable
    /// checkpoint callback.
    pub(super) fn send_prompt_to_agent_for_with_timing(
        &mut self,
        cid: &AgentId,
        timing: Option<PromptMaterializationTiming>,
    ) -> Option<AgentPromptId> {
        if self.agent_has_open_foreground_tool_round(cid) {
            return None;
        }
        let (
            owned_prompt_id,
            owned_model,
            owned_operation,
            runtime_incarnation,
            agent_id,
            originator,
        ) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| {
                let (prompt_id, model, operation) = match &agent.dispatch.activation_dispatch {
                    path_crate_agent::ActivationDispatchState::Running {
                        compact_prompt_id,
                        model,
                        ..
                    } => (
                        compact_prompt_id.clone(),
                        Some(model.clone()),
                        tau_proto::PromptOperation::StandaloneCompaction,
                    ),
                    path_crate_agent::ActivationDispatchState::DispatchUncertain {
                        agent_prompt_id,
                        model,
                        operation,
                        ..
                    } => (
                        agent_prompt_id.clone(),
                        model.clone(),
                        operation.as_ref().copied()?,
                    ),
                    _ => return None,
                };
                Some((
                    prompt_id,
                    model,
                    operation,
                    agent.identity.runtime_incarnation,
                    agent.identity.agent_id.clone()?,
                    agent.identity.originator.clone(),
                ))
            })?;
        if self
            .prompt_coordination
            .prompt_runtime
            .pending_dispatches
            .contains(&owned_prompt_id)
        {
            return None;
        }
        let owned_model = match owned_model {
            Some(model) if self.provider_runtime.model_routes.contains_key(&model) => model,
            model => {
                self.terminalize_unroutable_owned_dispatch(cid, model.as_ref());
                return None;
            }
        };
        let admission = tau_proto::AgentPromptStarted {
            agent_prompt_id: owned_prompt_id.clone(),
            agent_id: agent_id.clone(),
            session_id: self.session_runtime.current_session_id.clone(),
            model: owned_model,
            model_params: Some(tau_proto::ModelParams::default()),
            outer_turn_id: None,
            operation: owned_operation,
            originator,
            ctx_id: None,
        };
        let Some(tree) = self.session_runtime.agent_store.agent(agent_id.as_str()) else {
            self.terminalize_owned_dispatch_error(
                cid,
                "prompt materialization lacks one unmaterialized durable owner".to_owned(),
            );
            return None;
        };
        if tree.prompt_started(&owned_prompt_id).is_some() {
            return None;
        }
        if !tree.prompt_started_can_materialize(&admission) {
            self.terminalize_owned_dispatch_error(
                cid,
                "prompt materialization lacks one unmaterialized durable owner".to_owned(),
            );
            return None;
        }
        let prompt = self.prepare_agent_prompt_for_dispatch_timed(cid, timing.as_ref())?;
        self.ensure_outer_turn_started(cid);
        if prompt.operation == tau_proto::PromptOperation::Inference
            && self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .is_none_or(|agent| agent.turn.outer_turn.active_id().is_none())
        {
            self.terminalize_owned_dispatch_error(
                cid,
                "ordinary inference lacks durable outer-turn correlation".to_owned(),
            );
            return None;
        }
        let agent_prompt_id = prompt.agent_prompt_id.clone();
        if agent_prompt_id != owned_prompt_id
            || !self
                .prompt_coordination
                .prompt_runtime
                .pending_dispatches
                .insert(agent_prompt_id.clone())
        {
            self.terminalize_owned_dispatch_error(
                cid,
                "prompt materialization did not retain its unique durable owner".to_owned(),
            );
            return None;
        }
        let mut started = tau_proto::AgentPromptStarted::from(&prompt);
        if started.operation == tau_proto::PromptOperation::Inference {
            started.outer_turn_id = self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| agent.turn.outer_turn.active_id().cloned());
        }
        let provider_connection_id = self
            .provider_runtime
            .model_routes
            .get(&prompt.model)
            .cloned()
            .expect("owned model route was validated before materialization");
        self.enqueue_publish(
            None,
            Event::AgentPromptStarted(started.clone()),
            false,
            true,
            Some(ConversationHeadSync {
                cid: cid.clone(),
                agent_id: Some(agent_id),
                session_generation: self.session_runtime.current_session_generation,
                fold_parent: None,
                suppress_activation_dispatch: true,
                continuation: Some(PostCommitContinuation::PromptMaterialization(
                    PromptDispatchContinuation {
                        authority: interception::PromptDispatchAuthority {
                            started,
                            provider_connection_id,
                            runtime_incarnation,
                            materialization_timing: timing,
                        },
                        prompt: path_std_sync::Arc::new(prompt),
                    },
                )),
                notify_watchers: false,
            }),
        );
        Some(agent_prompt_id)
    }

    /// Commit prompt-start authority before one owned output-length successor
    /// fails locally without provider delivery.
    pub(super) fn terminalize_output_length_before_prompt_start(
        &mut self,
        cid: &AgentId,
        message: String,
    ) -> bool {
        let Some((agent_id, originator, continuation)) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| {
                let path_crate_agent::OutputLengthContinuationState::Active(continuation) =
                    &agent.turn.output_length_continuation
                else {
                    return None;
                };
                Some((
                    agent.identity.agent_id.clone()?,
                    agent.identity.originator.clone(),
                    continuation.clone(),
                ))
            })
        else {
            return false;
        };
        let started = tau_proto::AgentPromptStarted {
            agent_prompt_id: continuation.plan.agent_prompt_id.clone(),
            agent_id: agent_id.clone(),
            session_id: self.session_runtime.current_session_id.clone(),
            model: continuation.plan.dispatch.model.clone(),
            model_params: Some(tau_proto::ModelParams::default()),
            outer_turn_id: Some(continuation.plan.owner.outer_turn_id.clone()),
            operation: continuation.plan.dispatch.operation,
            originator: originator.clone(),
            ctx_id: None,
        };
        let response = ProviderResponseFinished {
            automatic_compaction_decision: None,
            agent_prompt_id: continuation.plan.agent_prompt_id.clone(),
            agent_id: agent_id.clone(),
            output_items: Vec::new(),
            stop_reason: ProviderStopReason::Error,
            error: Some(message),
            failure_kind: Some(tau_proto::ProviderFailureKind::Unknown),
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::ContinuationTerminal {
                outer_turn_id: continuation.plan.owner.outer_turn_id.clone(),
                source_agent_prompt_id: continuation.plan.owner.source_agent_prompt_id.clone(),
                ordinal: continuation.plan.owner.ordinal,
                outcome: tau_proto::OutputLengthContinuationOutcome::Failed,
                outer_turn_finish_owed: true,
            },
            originator,
            usage: None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        };
        let prompt_start_committed = self
            .session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .is_some_and(|tree| {
                tree.prompt_started(&continuation.plan.agent_prompt_id)
                    .is_some()
            });
        self.prompt_coordination
            .prompt_runtime
            .local_route_failures
            .insert(response.agent_prompt_id.clone());
        if prompt_start_committed {
            if self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .is_some_and(|agent| agent.dispatch.pending_cancel.is_some())
            {
                self.prompt_coordination
                    .prompt_runtime
                    .local_route_failures
                    .remove(&response.agent_prompt_id);
                self.finalize_canceled_in_flight_prompt(cid);
            } else {
                let completion = Some(AgentPublishCompletion::OutputLengthContinuation {
                    batch_parent: self
                        .selected_head_for_agent(cid)
                        .unwrap_or(tau_proto::AgentHead::Root),
                    reducer: CommittedOutputLengthContinuation {
                        response: Box::new(response.clone()),
                        assistant_text: None,
                    },
                    owned_publication: None,
                });
                self.publish_finished_response_for_agent(cid, None, &response, completion, false);
            }
            return true;
        }
        let batch_parent = self
            .selected_head_for_agent(cid)
            .unwrap_or(tau_proto::AgentHead::Root);
        self.enqueue_publish(
            None,
            Event::AgentPromptStarted(started),
            false,
            true,
            Some(ConversationHeadSync {
                cid: cid.clone(),
                agent_id: Some(agent_id),
                session_generation: self.session_runtime.current_session_generation,
                fold_parent: None,
                suppress_activation_dispatch: true,
                continuation: Some(PostCommitContinuation::AgentPublish(Box::new(
                    AgentPublishCompletion::OutputLengthPreDeliveryFailure {
                        batch_parent,
                        response: Box::new(response),
                        owned_publication: None,
                    },
                ))),
                notify_watchers: false,
            }),
        );
        true
    }

    /// Durably fails start- or checkpoint-owned work when its exact
    /// provider-qualified model no longer has a route, before any provider
    /// receives the request.
    pub(super) fn terminalize_unroutable_owned_dispatch(
        &mut self,
        cid: &AgentId,
        model: Option<&ModelId>,
    ) {
        let message = model.map_or_else(
            || "checkpoint has no provider-qualified model".to_owned(),
            |model| format!("checkpointed model `{model}` is unavailable"),
        );
        if self.terminalize_output_length_before_prompt_start(cid, message) {
            return;
        }
        let Some(agent) = self.agent_runtime.agent_registry.agents.get(cid) else {
            return;
        };
        let agent_id = agent.identity.agent_id.clone();
        let originator = agent.identity.originator.clone();
        let output_length_owner = match &agent.turn.output_length_continuation {
            path_crate_agent::OutputLengthContinuationState::Active(continuation) => Some((
                continuation.plan.agent_prompt_id.clone(),
                continuation.plan.owner.clone(),
            )),
            _ => None,
        };
        let failure = match &agent.dispatch.activation_dispatch {
            path_crate_agent::ActivationDispatchState::Running {
                id,
                cut,
                resume_through,
                ..
            } => agent_id.map(|agent_id| {
                Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
                    agent_id,
                    transaction_id: id.clone(),
                    cut: *cut,
                    reason: tau_proto::StandaloneCompactionFailureReason::RouteFailed,
                    resume_through: *resume_through,
                    context_retreat: None,
                    incomplete_response: None,
                })
            }),
            path_crate_agent::ActivationDispatchState::DispatchUncertain {
                agent_prompt_id,
                ..
            } => agent_id.map(|agent_id| {
                let output_length_disposition = output_length_owner
                    .as_ref()
                    .filter(|(prompt_id, _)| prompt_id == agent_prompt_id)
                    .map_or(tau_proto::OutputLengthDisposition::None, |(_, owner)| {
                        tau_proto::OutputLengthDisposition::ContinuationTerminal {
                            outer_turn_id: owner.outer_turn_id.clone(),
                            source_agent_prompt_id: owner.source_agent_prompt_id.clone(),
                            ordinal: owner.ordinal,
                            outcome: tau_proto::OutputLengthContinuationOutcome::Failed,
                            outer_turn_finish_owed: true,
                        }
                    });
                Event::ProviderResponseFinished(ProviderResponseFinished {
                    automatic_compaction_decision: None,
                    estimated_api_cost_rates: None,
                    estimated_api_cost_increment: None,

                    agent_prompt_id: agent_prompt_id.clone(),
                    agent_id,
                    output_items: Vec::new(),
                    stop_reason: ProviderStopReason::Error,
                    error: Some(model.map_or_else(
                        || "checkpoint has no provider-qualified model".to_owned(),
                        |model| format!("checkpointed model `{model}` is unavailable"),
                    )),
                    failure_kind: Some(tau_proto::ProviderFailureKind::Unknown),
                    context_limit_telemetry: None,
                    recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                    output_length_disposition,
                    originator,
                    usage: None,
                    compaction_original_input_tokens: None,
                    compaction_output_tokens: None,
                    backend: None,
                    provider_attempt: Default::default(),
                    provider_response_id: None,
                    ws_pool_delta: None,
                })
            }),
            _ => None,
        };
        if let Some(Event::ProviderResponseFinished(response)) = failure.as_ref() {
            self.prompt_coordination
                .prompt_runtime
                .local_route_failures
                .insert(response.agent_prompt_id.clone());
            self.invalidate_working_status_after_unsuccessful_terminal(cid);
        }
        if let Some(Event::ProviderResponseFinished(response)) = failure.as_ref()
            && matches!(
                response.output_length_disposition,
                tau_proto::OutputLengthDisposition::ContinuationTerminal { .. }
            )
        {
            let completion = Some(AgentPublishCompletion::OutputLengthContinuation {
                batch_parent: self
                    .selected_head_for_agent(cid)
                    .unwrap_or(tau_proto::AgentHead::Root),
                reducer: CommittedOutputLengthContinuation {
                    response: Box::new(response.clone()),
                    assistant_text: None,
                },
                owned_publication: None,
            });
            self.publish_finished_response_for_agent(cid, None, response, completion, false);
            return;
        }
        if let Some(failure) = failure {
            self.publish_for_agent(cid, failure);
        }
    }

    /// Durably close owned dispatch work that became invalid after its
    /// pre-check but before the intercepted dispatch checkpoint committed.
    pub(super) fn terminalize_owned_dispatch_error(&mut self, cid: &AgentId, message: String) {
        if self.terminalize_output_length_before_prompt_start(cid, message.clone()) {
            return;
        }
        let Some(agent) = self.agent_runtime.agent_registry.agents.get(cid) else {
            return;
        };
        let Some(agent_id) = agent.identity.agent_id.clone() else {
            return;
        };
        let originator = agent.identity.originator.clone();
        let failure = match &agent.dispatch.activation_dispatch {
            path_crate_agent::ActivationDispatchState::Running {
                id,
                cut,
                resume_through,
                ..
            } => {
                Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
                    agent_id,
                    transaction_id: id.clone(),
                    cut: *cut,
                    reason: tau_proto::StandaloneCompactionFailureReason::RouteFailed,
                    resume_through: *resume_through,
                    context_retreat: None,
                    incomplete_response: None,
                })
            }
            path_crate_agent::ActivationDispatchState::DispatchUncertain {
                agent_prompt_id,
                ..
            } => {
                let agent_prompt_id = agent_prompt_id.clone();
                self.prompt_coordination
                    .prompt_runtime
                    .local_route_failures
                    .insert(agent_prompt_id.clone());
                Event::ProviderResponseFinished(ProviderResponseFinished {
                    automatic_compaction_decision: None,
                    estimated_api_cost_rates: None,
                    estimated_api_cost_increment: None,

                    agent_prompt_id,
                    agent_id,
                    output_items: Vec::new(),
                    stop_reason: ProviderStopReason::Error,
                    error: Some(message),
                    failure_kind: Some(tau_proto::ProviderFailureKind::Unknown),
                    context_limit_telemetry: None,
                    recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                    output_length_disposition: tau_proto::OutputLengthDisposition::None,
                    originator,
                    usage: None,
                    compaction_original_input_tokens: None,
                    compaction_output_tokens: None,
                    backend: None,
                    provider_attempt: Default::default(),
                    provider_response_id: None,
                    ws_pool_delta: None,
                })
            }
            _ => return,
        };
        if matches!(failure, Event::ProviderResponseFinished(_)) {
            self.invalidate_working_status_after_unsuccessful_terminal(cid);
        }
        self.publish_for_agent(cid, failure);
    }

    /// Invalidate a Working report when a synthetic unsuccessful terminal
    /// bypasses the ordinary provider-response gate.
    pub(super) fn invalidate_working_status_after_unsuccessful_terminal(&mut self, cid: &AgentId) {
        let changed = self
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(cid)
            .is_some_and(|agent| agent.turn.work_status.invalidate_working());
        if changed {
            self.notify_work_status_transition(cid);
        }
    }

    /// Builds one prompt request and records the live in-flight bookkeeping
    /// needed to route the corresponding provider response. The prompt payload
    /// is returned to the caller and retained only by the compact prompt fact's
    /// live continuation; semantic persistence never stores it.
    pub(super) fn prepare_agent_prompt_for_dispatch(
        &mut self,
        cid: &AgentId,
    ) -> Option<AgentPromptCreated> {
        self.prepare_agent_prompt_for_dispatch_timed(cid, None)
    }

    /// Builds a provider prompt while optionally recording content-free local
    /// materialization diagnostics.
    fn prepare_agent_prompt_for_dispatch_timed(
        &mut self,
        cid: &AgentId,
        timing: Option<&PromptMaterializationTiming>,
    ) -> Option<AgentPromptCreated> {
        let _ = self.ensure_agent_id_for_agent(cid);
        let conv = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .expect("prepare_agent_prompt_for_dispatch: unknown agent id");
        let originator = conv.identity.originator.clone();
        let role_name = self.role_name_for_agent(conv);
        let (prompt_model, owned_operation) = match &conv.dispatch.activation_dispatch {
            path_crate_agent::ActivationDispatchState::Running { model, .. } => (
                Some(model.clone()),
                tau_proto::PromptOperation::StandaloneCompaction,
            ),
            path_crate_agent::ActivationDispatchState::DispatchUncertain {
                model,
                operation,
                ..
            } => (
                model.clone(),
                operation.unwrap_or(tau_proto::PromptOperation::Inference),
            ),
            _ => (
                self.model_for_agent_role(conv),
                tau_proto::PromptOperation::Inference,
            ),
        };
        let prompt_params = prompt_model
            .as_ref()
            .map(|model| self.params_for_role_model(&role_name, model))
            .unwrap_or_default();
        let Some(model) = prompt_model else {
            self.emit_info(&format!(
                "role `{role_name}` has no available model — use :role to pick a role, :model <provider>/<model> to pick an agent model, or enable a provider"
            ));
            return None;
        };
        // Non-tool extension side agents (`std-notifications`' idle summary,
        // etc.) must not execute tools. Provider `tool_choice: none` is the
        // upstream authority; local rejection alone cannot contain hosted tools.
        let is_non_tool_ext_query = Self::agent_uses_non_tool_prompt_surface(conv);
        let tool_choice = if is_non_tool_ext_query {
            tau_proto::ToolChoice::None
        } else {
            tau_proto::ToolChoice::Auto
        };
        // Legacy cache-sharing hint for older provider implementations. The
        // first-party ChatGPT/Codex provider now derives cache keys only from
        // base URL and target agent id, so prompt originator and this flag do
        // not split cache buckets.
        let share_user_cache_key = is_non_tool_ext_query;
        // Walk the agent's *own* branch, not whatever tree.head
        // currently points at. With multiple side agents
        // running concurrently their tree mutations interleave, so
        // tree.head is an unreliable signal for "where this
        // conversation lives". Reading from `conv.head` keeps the
        // assembled prompt scoped to this agent's history and
        // prevents orphan ToolUse blocks from cross-branch state.
        let compaction_transaction = match &conv.dispatch.activation_dispatch {
            path_crate_agent::ActivationDispatchState::Running {
                id,
                cut,
                resume_through,
                ..
            } => Some((id.clone(), *cut, *resume_through)),
            _ => None,
        };
        let checkpointed_inference = match &conv.dispatch.activation_dispatch {
            path_crate_agent::ActivationDispatchState::DispatchUncertain {
                owner,
                agent_prompt_id,
                through,
                activation_cut,
                ..
            } => {
                tracing::trace!(
                    target: "tau_harness",
                    transaction_id = ?owner.transaction_id(),
                    agent_prompt_id = %agent_prompt_id,
                    activation_cut = ?activation_cut,
                    "materializing checkpointed inference"
                );
                Some((agent_prompt_id.clone(), *through))
            }
            _ => None,
        };
        let reserved_compact_prompt_id = match &conv.dispatch.activation_dispatch {
            path_crate_agent::ActivationDispatchState::Running {
                compact_prompt_id: prompt_id,
                ..
            } => Some(prompt_id.clone()),
            _ => None,
        };
        let head = conv.selected_prompt_context_head();
        let standalone_window = match &conv.dispatch.activation_dispatch {
            path_crate_agent::ActivationDispatchState::Running {
                cut,
                resume_through,
                ..
            } => Some((resume_through.unwrap_or(*cut), *cut)),
            _ => None,
        };

        let agent_id_for_tree = conv.identity.agent_id.clone();
        let tree = agent_id_for_tree
            .as_deref()
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id));
        if let Some(message) = self.shell_tool_style_error(Some(&model)) {
            self.emit_harness_failure(&message);
            self.terminalize_owned_dispatch_error(cid, message);
            return None;
        }
        let stage_started = stage_start(timing);
        let prompt_context = tree
            .and_then(|tree| {
                standalone_window.map_or_else(
                    || Some(assemble_prompt_context_from(tree, head)),
                    |(through, cut)| {
                        crate::prompt::assemble_prompt_context_prefix_from(
                            tree,
                            through.as_option(),
                            cut,
                        )
                    },
                )
            })
            .unwrap_or_else(|| crate::prompt::AssembledPromptContext {
                context: tau_proto::PromptContext::default(),
                contains_payload_envelope_provenance_projection: false,
            });
        let contains_payload_envelope_provenance_projection =
            prompt_context.contains_payload_envelope_provenance_projection;
        let mut context = prompt_context.context;
        if let Some(initialization_block) =
            tree.and_then(crate::prompt::initialization_agents_context_block)
        {
            context.blocks.insert(0, initialization_block);
        }
        if compaction_transaction.is_some() {
            context.blocks.push(tau_proto::ContextBlock::UserInput(
                tau_proto::UserInputBlock {
                    items: vec![ContextItem::CompactionTrigger],
                },
            ));
        }
        if let Some(timing) = timing {
            timing.record(
                MaterializationStage::BranchContext,
                stage_started
                    .expect("enabled timing has a stage start")
                    .elapsed(),
            );
        }
        let operation = owned_operation;
        let durable_agent_id = agent_id_for_tree.clone();
        let surface = match self.prepare_prompt_surface_for_dispatch_timed(
            &role_name,
            durable_agent_id.as_ref(),
            durable_agent_id.as_ref(),
            &model,
            is_non_tool_ext_query,
            contains_payload_envelope_provenance_projection,
            timing,
        ) {
            Ok(surface) => surface,
            Err(PromptSurfaceError::DuplicateToolName(name)) => {
                let message = format!(
                    "cannot dispatch prompt for role `{role_name}`: effective tool surface contains duplicate model-visible name `{name}`"
                );
                self.emit_harness_failure(&message);
                self.terminalize_owned_dispatch_error(cid, message);
                return None;
            }
            Err(PromptSurfaceError::Render(error)) => {
                let message =
                    format!("failed to render system prompt for role `{role_name}`: {error}");
                self.emit_harness_failure(&message);
                self.terminalize_owned_dispatch_error(cid, message);
                return None;
            }
            Err(PromptSurfaceError::WebUnavailable(message)) => {
                self.emit_harness_failure(&message);
                self.terminalize_owned_dispatch_error(cid, message);
                return None;
            }
        };
        let MaterializedPromptSurface {
            tool_specs,
            tool_definitions: tools,
            hosted_tools,
            invocation_policies: tool_invocation_policies,
            system_prompt,
        } = surface;
        let durable_agent_id = agent_id_for_tree.as_deref().unwrap_or(cid.as_ref());
        let agent_prompt_id = reserved_compact_prompt_id
            .or_else(|| checkpointed_inference.map(|(prompt_id, _)| prompt_id))
            .unwrap_or_else(|| {
                let prompt_index = self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get_mut(cid)
                    .expect("prepare_agent_prompt_for_dispatch: unknown agent id")
                    .dispatch
                    .next_prompt_index;
                if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                    agent.dispatch.next_prompt_index += 1;
                }
                AgentPromptId::parse(format!("ap-{durable_agent_id}-{prompt_index}"))
                    .expect("known-safe AgentPromptId must be valid")
            });
        self.prompt_coordination
            .prompt_runtime
            .agents
            .insert(agent_prompt_id.clone(), cid.clone());
        let ctx_id = self
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(cid)
            .and_then(|c| c.dispatch.next_ctx_id.take());
        if let Some(c) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            c.dispatch.in_flight_prompt = Some(agent_prompt_id.clone());
        }
        self.set_agent_turn_state(
            cid,
            AgentTurnState::AgentThinking {
                agent_prompt_id: agent_prompt_id.clone(),
            },
        );

        let stage_started = stage_start(timing);
        if operation != tau_proto::PromptOperation::StandaloneCompaction {
            self.session_runtime
                .current_session_state
                .token_usage
                .start_request(&model);
        }
        self.prompt_coordination
            .prompt_runtime
            .models
            .insert(agent_prompt_id.clone(), model.clone());
        let context_limit_snapshot = self.prompt_context_limit_snapshot(cid, &model, operation);
        self.prompt_coordination
            .prompt_runtime
            .context_limits
            .insert(agent_prompt_id.clone(), context_limit_snapshot);
        let role_name = self.role_name_for_agent_id(cid);
        let context_size_alerts = self
            .config
            .available_roles
            .get(&role_name)
            .map(|role| role.context_size_alerts.clone())
            .unwrap_or_default();
        self.prompt_coordination
            .prompt_runtime
            .context_size_alerts
            .insert(agent_prompt_id.clone(), context_size_alerts);
        let compactions = self
            .config
            .available_roles
            .get(&role_name)
            .map(|role| role.compactions.clone())
            .unwrap_or_default();
        self.prompt_coordination
            .prompt_runtime
            .compaction_policies
            .insert(agent_prompt_id.clone(), compactions);
        self.prompt_coordination.prompt_runtime.operations.insert(
            agent_prompt_id.clone(),
            (
                operation,
                compaction_transaction
                    .as_ref()
                    .is_some_and(|(_, _, resume)| resume.is_some()),
            ),
        );
        self.prompt_coordination
            .prompt_runtime
            .tool_specs
            .insert(agent_prompt_id.clone(), tool_specs);
        self.prompt_coordination
            .prompt_runtime
            .tool_invocation_policies
            .insert(agent_prompt_id.clone(), tool_invocation_policies);
        let session_id = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .expect("agent still exists")
            .identity
            .session_id
            .clone();
        let agent_id = self
            .ensure_agent_id_for_agent(cid)
            .expect("agent has durable id");
        let compaction = self.compaction_context_for_agent(cid, &model);
        if let Some(timing) = timing {
            timing.record(
                MaterializationStage::Accounting,
                stage_started
                    .expect("enabled timing has a stage start")
                    .elapsed(),
            );
            #[cfg(test)]
            note_count_work();
            let context_items = context.flatten_iter().count();
            let images = context
                .flatten_iter()
                .filter_map(|item| match item {
                    ContextItem::ToolResult(result) => Some(result.provider_content.len()),
                    _ => None,
                })
                .fold(0_usize, usize::saturating_add);
            let schema_bytes = tools
                .iter()
                .filter_map(|tool| tool.parameters.as_ref())
                .map(serialized_json_len)
                .fold(0_usize, usize::saturating_add);
            timing.set_counts(MaterializationCounts {
                tools: tools.len(),
                schema_bytes,
                context_blocks: context.blocks.len(),
                context_items,
                images,
                recipients: 0,
            });
        }
        Some(AgentPromptCreated {
            agent_prompt_id,
            agent_id,
            session_id,
            system_prompt,
            context,
            tools,
            tools_ref: None,
            hosted_tools,
            model,
            model_params: prompt_params,
            tool_choice,
            originator,
            share_user_cache_key,
            ctx_id,
            compaction,
            operation,
        })
    }

    /// Validate the current prompt surface before committing the durable
    /// inference-dispatch checkpoint.
    pub(crate) fn validate_prompt_render_for_dispatch(&mut self, cid: &AgentId) -> bool {
        let Some(conv) = self.agent_runtime.agent_registry.agents.get(cid) else {
            return false;
        };
        let role_name = self.role_name_for_agent(conv);
        let model = match &conv.turn.output_length_continuation {
            path_crate_agent::OutputLengthContinuationState::Planned(continuation) => {
                Some(continuation.dispatch.model.clone())
            }
            path_crate_agent::OutputLengthContinuationState::OwnerReady(continuation)
            | path_crate_agent::OutputLengthContinuationState::OwnerPending(continuation)
            | path_crate_agent::OutputLengthContinuationState::Active(continuation) => {
                Some(continuation.plan.dispatch.model.clone())
            }
            path_crate_agent::OutputLengthContinuationState::None
            | path_crate_agent::OutputLengthContinuationState::Spent { .. } => {
                self.model_for_agent_role(conv)
            }
        };
        let Some(model) = model else {
            return true;
        };
        let is_non_tool_ext_query = Self::agent_uses_non_tool_prompt_surface(conv);
        if let Some(message) = self.shell_tool_style_error(Some(&model)) {
            self.emit_harness_failure(&message);
            self.fail_initial_prompt_materialization(
                cid,
                "failed to validate initial prompt tool surface",
            );
            return false;
        }
        let durable_agent_id = conv.identity.agent_id.clone();
        let contains_payload_envelope_provenance_projection = conv
            .identity
            .agent_id
            .as_deref()
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .map(|tree| {
                crate::prompt::active_prompt_context_contains_payload_envelope_provenance_projection(
                    tree,
                    conv.selected_prompt_context_head(),
                )
            })
            .unwrap_or(false);
        match self.prepare_prompt_surface_for_dispatch_timed(
            &role_name,
            durable_agent_id.as_ref(),
            durable_agent_id.as_ref(),
            &model,
            is_non_tool_ext_query,
            contains_payload_envelope_provenance_projection,
            None,
        ) {
            Ok(_) => true,
            Err(error) => {
                let message = match error {
                    PromptSurfaceError::DuplicateToolName(name) => format!(
                        "cannot dispatch prompt for role `{role_name}`: effective tool surface contains duplicate model-visible name `{name}`"
                    ),
                    PromptSurfaceError::Render(error) => format!(
                        "cannot dispatch prompt for role `{role_name}` until its template is repaired: {error}"
                    ),
                    PromptSurfaceError::WebUnavailable(message) => message,
                };
                self.emit_harness_failure(&message);
                self.fail_initial_prompt_materialization(
                    cid,
                    "failed to validate initial prompt tool surface",
                );
                false
            }
        }
    }

    pub(super) fn role_name_for_agent(&self, conv: &Agent) -> String {
        conv.identity
            .role
            .clone()
            .unwrap_or_else(|| self.config.selected_role.clone())
    }

    pub(super) fn role_name_for_agent_id(&self, cid: &AgentId) -> String {
        self.agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|conv| conv.identity.role.clone())
            .unwrap_or_else(|| self.config.selected_role.clone())
    }

    pub(super) fn model_for_agent_role(&self, conv: &Agent) -> Option<ModelId> {
        if let Some(model) = conv.identity.model_override.clone()
            && self.provider_runtime.model_routes.contains_key(&model)
        {
            return Some(model);
        }
        let role_name = self.role_name_for_agent(conv);
        model_for_role(
            &self.provider_runtime.model_info,
            &self.config.available_roles,
            &role_name,
        )
    }

    pub(crate) fn selected_model_params(&self) -> tau_proto::ModelParams {
        self.config
            .selected_model
            .as_ref()
            .map(|model| self.params_for_role_model(&self.config.selected_role, model))
            .unwrap_or_default()
    }

    pub(super) fn params_for_role_model(
        &self,
        role_name: &str,
        model: &ModelId,
    ) -> tau_proto::ModelParams {
        selected_params_for_role(
            &self.provider_runtime.model_info,
            &self.config.available_roles,
            role_name,
            model,
        )
    }

    #[cfg(test)]
    pub(super) fn build_system_prompt_for_role(&self, role_name: &str) -> String {
        let model = model_for_role(
            &self.provider_runtime.model_info,
            &self.config.available_roles,
            role_name,
        );
        let specs = self.gather_effective_tool_specs_for_role_model(role_name, model.as_ref());
        self.try_build_system_prompt_for_role_and_agent(
            role_name,
            None,
            None,
            &specs,
            model.as_ref(),
            false,
        )
        .expect("configured role prompt should render")
    }

    #[cfg(test)]
    pub(super) fn build_system_prompt_for_role_preview(
        &self,
        role_name: &str,
        context_agent_id: &tau_proto::AgentId,
    ) -> Result<String, handlebars::RenderError> {
        let model = model_for_role(
            &self.provider_runtime.model_info,
            &self.config.available_roles,
            role_name,
        );
        let specs = self.gather_effective_tool_specs_for_role_model(role_name, model.as_ref());
        self.build_system_prompt_for_role_preview_with_snapshot(
            role_name,
            context_agent_id,
            &specs,
            model.as_ref(),
        )
    }

    /// Renders a preview from one already-resolved model and tool snapshot.
    pub(super) fn build_system_prompt_for_role_preview_with_snapshot(
        &self,
        role_name: &str,
        context_agent_id: &tau_proto::AgentId,
        specs: &[tau_proto::ToolSpec],
        model: Option<&ModelId>,
    ) -> Result<String, handlebars::RenderError> {
        let preview_agent_id = crate::parse_agent_id(RENDERED_PROMPT_PREVIEW_AGENT_ID);
        self.try_build_system_prompt_for_role_and_agent(
            role_name,
            Some(&preview_agent_id),
            Some(context_agent_id),
            specs,
            model,
            false,
        )
    }

    /// Resolve and render one provider-visible prompt surface from a single
    /// sorted provider snapshot.
    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub(super) fn prepare_prompt_surface_for_dispatch(
        &self,
        role_name: &str,
        agent_id: Option<&tau_proto::AgentId>,
        context_agent_id: Option<&tau_proto::AgentId>,
        model: &ModelId,
        hide_tool_capabilities: bool,
        contains_payload_envelope_provenance_projection: bool,
    ) -> Result<(Vec<tau_proto::ToolSpec>, Vec<ToolDefinition>, String), PromptSurfaceError> {
        let surface = self.prepare_prompt_surface_for_dispatch_timed(
            role_name,
            agent_id,
            context_agent_id,
            model,
            hide_tool_capabilities,
            contains_payload_envelope_provenance_projection,
            None,
        )?;
        Ok((
            surface.tool_specs,
            surface.tool_definitions,
            surface.system_prompt,
        ))
    }

    /// Timed form of prompt-surface preparation used only by live provider
    /// materialization.
    #[allow(clippy::too_many_arguments)]
    fn prepare_prompt_surface_for_dispatch_timed(
        &self,
        role_name: &str,
        agent_id: Option<&tau_proto::AgentId>,
        context_agent_id: Option<&tau_proto::AgentId>,
        model: &ModelId,
        hide_tool_capabilities: bool,
        contains_payload_envelope_provenance_projection: bool,
        timing: Option<&PromptMaterializationTiming>,
    ) -> Result<MaterializedPromptSurface, PromptSurfaceError> {
        let stage_started = stage_start(timing);
        let providers = self.sorted_prompt_tool_providers();
        let mut specs = self.gather_effective_tool_specs_for_role_model_from_providers(
            role_name,
            Some(model),
            &providers,
        );
        let mut hosted_tools = Vec::new();
        let mut invocation_policies = HashMap::new();
        if let Some(policy) = self
            .config
            .available_roles
            .get(role_name)
            .map(|role| &role.web_tools)
            && hide_tool_capabilities
        {
            suppress_declared_web_candidates(policy, &mut specs);
        } else if let (Some(policy), Some(model_info)) = (
            self.config
                .available_roles
                .get(role_name)
                .map(|role| &role.web_tools),
            self.provider_runtime.model_info.get(model),
        ) {
            let compiled = compile_web_tools(policy, model_info, &specs)
                .map_err(PromptSurfaceError::WebUnavailable)?;
            let declared_candidates = policy.declared_tool_names().collect::<HashSet<_>>();
            specs.retain(|spec| {
                !declared_candidates.contains(&spec.name)
                    || compiled.retained_tools.contains(&spec.name)
            });
            hosted_tools = compiled.hosted_tools;
            invocation_policies = compiled.invocation_policies;
        }
        if hosted_web_search_collides(&hosted_tools, &specs) {
            return Err(PromptSurfaceError::DuplicateToolName(
                "web_search".to_owned(),
            ));
        }
        if let Some(name) = duplicate_model_visible_tool_name(&specs) {
            return Err(PromptSurfaceError::DuplicateToolName(name.to_string()));
        }
        let mut capability_specs = if hide_tool_capabilities {
            Vec::new()
        } else {
            specs.clone()
        };
        if !hide_tool_capabilities && !hosted_tools.is_empty() {
            capability_specs.push(tau_proto::ToolSpec {
                name: ToolName::new("web_search"),
                model_visible_name: None,
                description: Some("Search the web through the exact model provider.".to_owned()),
                tool_type: tau_proto::ToolType::Function,
                parameters: None,
                format: None,
                tags: vec![tau_proto::ToolTag::new(tau_proto::WEB_SEARCH_TOOL_TAG)],
                enabled_by_default: true,
                background_support: None,
                examples: Vec::new(),
            });
        }
        let effective_tool_names = capability_specs
            .iter()
            .map(|spec| spec.name.clone())
            .collect::<HashSet<_>>();
        let tools = self.tool_definitions_from_specs(&specs);
        if let Some(timing) = timing {
            timing.record(
                MaterializationStage::ToolsSchema,
                stage_started
                    .expect("enabled timing has a stage start")
                    .elapsed(),
            );
        }
        let prompt = self
            .try_build_system_prompt_for_role_and_agent_with_snapshot_timed(
                role_name,
                agent_id,
                context_agent_id,
                &capability_specs,
                Some(model),
                contains_payload_envelope_provenance_projection,
                &providers,
                &effective_tool_names,
                timing,
            )
            .map_err(PromptSurfaceError::Render)?;
        Ok(MaterializedPromptSurface {
            tool_specs: specs,
            tool_definitions: tools,
            hosted_tools,
            invocation_policies,
            system_prompt: prompt,
        })
    }

    pub(super) fn try_build_system_prompt_for_role_and_agent(
        &self,
        role_name: &str,
        agent_id: Option<&tau_proto::AgentId>,
        context_agent_id: Option<&tau_proto::AgentId>,
        tool_specs: &[tau_proto::ToolSpec],
        model: Option<&ModelId>,
        contains_payload_envelope_provenance_projection: bool,
    ) -> Result<String, handlebars::RenderError> {
        let providers = self.sorted_prompt_tool_providers();
        let effective_tool_names = tool_specs
            .iter()
            .map(|spec| spec.name.clone())
            .collect::<HashSet<_>>();
        self.try_build_system_prompt_for_role_and_agent_with_snapshot(
            role_name,
            agent_id,
            context_agent_id,
            tool_specs,
            model,
            contains_payload_envelope_provenance_projection,
            &providers,
            &effective_tool_names,
        )
    }

    /// Render with the dispatch-owned sorted provider and effective-name
    /// snapshots rather than re-reading and re-sorting the live registry.
    #[allow(clippy::too_many_arguments)]
    fn try_build_system_prompt_for_role_and_agent_with_snapshot(
        &self,
        role_name: &str,
        agent_id: Option<&tau_proto::AgentId>,
        context_agent_id: Option<&tau_proto::AgentId>,
        tool_specs: &[tau_proto::ToolSpec],
        model: Option<&ModelId>,
        contains_payload_envelope_provenance_projection: bool,
        providers: &[&tau_core::ToolProvider],
        effective_tool_names: &HashSet<ToolName>,
    ) -> Result<String, handlebars::RenderError> {
        self.try_build_system_prompt_for_role_and_agent_with_snapshot_timed(
            role_name,
            agent_id,
            context_agent_id,
            tool_specs,
            model,
            contains_payload_envelope_provenance_projection,
            providers,
            effective_tool_names,
            None,
        )
    }

    /// Timed form that separates prompt-input projection from Handlebars work.
    #[allow(clippy::too_many_arguments)]
    fn try_build_system_prompt_for_role_and_agent_with_snapshot_timed(
        &self,
        role_name: &str,
        agent_id: Option<&tau_proto::AgentId>,
        context_agent_id: Option<&tau_proto::AgentId>,
        tool_specs: &[tau_proto::ToolSpec],
        model: Option<&ModelId>,
        contains_payload_envelope_provenance_projection: bool,
        providers: &[&tau_core::ToolProvider],
        effective_tool_names: &HashSet<ToolName>,
        timing: Option<&PromptMaterializationTiming>,
    ) -> Result<String, handlebars::RenderError> {
        let stage_started = stage_start(timing);
        if let Some(name) = duplicate_model_visible_tool_name(tool_specs) {
            return Err(handlebars::RenderError::from(
                handlebars::RenderErrorReason::Other(format!(
                    "effective tool surface contains duplicate model-visible name `{name}`"
                )),
            ));
        }
        let (prompt_fragments, tool_prompt_fragments) = self
            .gather_prompt_fragment_groups_for_role_snapshot(
                role_name,
                providers,
                effective_tool_names,
            );
        let visible_workdir_contributors = providers
            .iter()
            .copied()
            .filter(|provider| {
                provider
                    .tool
                    .tags
                    .iter()
                    .any(|tag| tag.as_str() == "shell:workdir")
                    && effective_tool_names.contains(&provider.tool.name)
            })
            .map(|provider| provider.connection_id.clone())
            .collect::<HashSet<_>>();
        let system_template = self.system_template_for_role(role_name)?;
        let skills = context_agent_id
            .and_then(|agent_id| {
                self.prompt_coordination
                    .context_discovery
                    .frozen_agents
                    .get(agent_id)
            })
            .map_or(
                &self.prompt_coordination.context_discovery.skills,
                |snapshot| &snapshot.skills,
            );
        let role_group = self.role_group_name_for_role(role_name);
        let template_context = match agent_id {
            Some(agent_id) => RolePromptTemplateContext::for_agent(role_name, agent_id),
            None => RolePromptTemplateContext::for_role(role_name),
        }
        .with_role_group(&role_group)
        .with_session_cwd(&self.session_runtime.project_root)
        .with_payload_envelope_provenance_notice(
            contains_payload_envelope_provenance_projection
                .then_some(PAYLOAD_ENVELOPE_PROVENANCE_NOTICE),
        );
        let agent_context = self
            .prompt_coordination
            .context_discovery
            .agent_context
            .template_value_filtered(context_agent_id, |key, contributor| {
                key.as_ref() != "workdir" || visible_workdir_contributors.contains(contributor)
            });
        let capabilities = path_crate_prompt::PromptCapabilities::new(
            tool_specs
                .iter()
                .map(|spec| self.tool_model_visible_name(spec).to_string()),
            self.extensions.enabled_names.iter().cloned().chain(
                self.extensions
                    .entries
                    .values()
                    .map(|entry| entry.name.to_string()),
            ),
            self.extensions
                .entries
                .values()
                .filter(|entry| entry.state == path_crate_extension::ExtensionState::Ready)
                .map(|entry| entry.name.to_string()),
        )
        .with_parallel_tool_calls(
            !tool_specs.is_empty()
                && model
                    .and_then(|model| self.provider_runtime.model_info.get(model))
                    .is_none_or(|info| info.supports_parallel_tool_calls),
        );
        if let Some(timing) = timing {
            timing.record(
                MaterializationStage::FragmentsSkillsContext,
                stage_started
                    .expect("enabled timing has a stage start")
                    .elapsed(),
            );
        }
        let stage_started = stage_start(timing);
        let rendered = try_build_system_prompt_with_engine(
            &self
                .prompt_coordination
                .context_discovery
                .prompt_template_engine,
            system_template,
            skills,
            &prompt_fragments,
            &tool_prompt_fragments,
            agent_context,
            template_context,
            capabilities,
        );
        if let Some(timing) = timing {
            timing.record(
                MaterializationStage::HandlebarsRender,
                stage_started
                    .expect("enabled timing has a stage start")
                    .elapsed(),
            );
        }
        rendered
    }

    pub(super) fn system_template_for_role(
        &self,
        role_name: &str,
    ) -> Result<&str, handlebars::RenderError> {
        let template_name = self
            .config
            .available_roles
            .get(role_name)
            .and_then(|role| role.prompt_override.as_deref())
            .unwrap_or(BUILT_IN_SYSTEM_TEMPLATE_NAME);
        self.prompt_coordination
            .context_discovery
            .system_prompt_templates
            .get(template_name)
            .map(String::as_str)
            .ok_or_else(|| {
                handlebars::RenderError::from(handlebars::RenderErrorReason::Other(format!(
                    "unknown system prompt template `{template_name}`"
                )))
            })
    }

    #[cfg(test)]
    pub(super) fn gather_prompt_fragments(&self) -> Vec<PromptFragment> {
        self.gather_prompt_fragments_for_role(&self.config.selected_role)
    }

    #[cfg(test)]
    pub(super) fn gather_prompt_fragments_for_role(&self, role_name: &str) -> Vec<PromptFragment> {
        let (fragments, tool_fragments) = self.gather_sourced_prompt_fragment_groups(role_name);
        sorted_prompt_fragments(fragments.into_iter().chain(tool_fragments.into_iter().map(
            |sourced| SourcedPromptFragment {
                source: sourced.source,
                fragment: sourced.fragment,
            },
        )))
    }

    /// Gather ordered fragments from an already-sorted provider snapshot.
    fn gather_prompt_fragment_groups_for_role_snapshot(
        &self,
        role_name: &str,
        providers: &[&tau_core::ToolProvider],
        effective_tool_names: &HashSet<ToolName>,
    ) -> (Vec<PromptFragment>, Vec<ToolPromptFragment>) {
        let (fragments, tool_fragments) = self
            .gather_sourced_prompt_fragment_groups_for_provider_snapshot(
                role_name,
                providers,
                Some(effective_tool_names),
            );
        (
            sorted_prompt_fragments(fragments),
            sorted_tool_prompt_fragments(tool_fragments),
        )
    }

    #[cfg(test)]
    pub(super) fn gather_sourced_prompt_fragment_groups(
        &self,
        role_name: &str,
    ) -> (Vec<SourcedPromptFragment>, Vec<SourcedToolPromptFragment>) {
        self.gather_sourced_prompt_fragment_groups_for_specs(role_name, None)
    }

    #[cfg(test)]
    pub(super) fn gather_sourced_prompt_fragment_groups_for_specs(
        &self,
        role_name: &str,
        effective_specs: Option<&[tau_proto::ToolSpec]>,
    ) -> (Vec<SourcedPromptFragment>, Vec<SourcedToolPromptFragment>) {
        let providers = self.sorted_prompt_tool_providers();
        let effective_tool_names = effective_specs.map(|specs| {
            specs
                .iter()
                .map(|spec| spec.name.clone())
                .collect::<HashSet<_>>()
        });
        self.gather_sourced_prompt_fragment_groups_for_provider_snapshot(
            role_name,
            &providers,
            effective_tool_names.as_ref(),
        )
    }

    /// Gather sourced fragments without taking another provider-registry
    /// snapshot.
    fn gather_sourced_prompt_fragment_groups_for_provider_snapshot(
        &self,
        role_name: &str,
        providers: &[&tau_core::ToolProvider],
        effective_tool_names: Option<&HashSet<ToolName>>,
    ) -> (Vec<SourcedPromptFragment>, Vec<SourcedToolPromptFragment>) {
        let provider_enabled = |provider: &tau_core::ToolProvider| {
            effective_tool_names.map_or_else(
                || self.is_tool_provider_enabled_for_role(provider, role_name),
                |names| names.contains(&provider.tool.name),
            )
        };
        let shell_workdir_visible = effective_tool_names.map_or_else(
            || {
                providers.iter().any(|provider| {
                    provider_enabled(provider)
                        && provider
                            .tool
                            .tags
                            .iter()
                            .any(|tag| tag.as_str() == "shell:workdir")
                })
            },
            |_| {
                providers.iter().copied().any(|provider| {
                    provider_enabled(provider)
                        && provider
                            .tool
                            .tags
                            .iter()
                            .any(|tag| tag.as_str() == "shell:workdir")
                })
            },
        );
        let mut fragments: Vec<_> = self
            .prompt_coordination
            .context_discovery
            .prompt_fragments
            .iter()
            .flat_map(|(connection_id, fragments)| {
                fragments
                    .values()
                    .map(move |fragment| SourcedPromptFragment {
                        source: PromptFragmentSource::Extension {
                            connection_id: connection_id.clone(),
                        },
                        fragment: fragment.clone(),
                    })
            })
            .collect();
        let mut saw_shell_workdir_fragment = false;
        fragments.retain(|sourced| {
            let PromptFragmentSource::Extension { .. } = &sourced.source else {
                return true;
            };
            if sourced.fragment.name != "shell.workdir" {
                return true;
            }
            if !shell_workdir_visible {
                return false;
            }
            if saw_shell_workdir_fragment {
                false
            } else {
                saw_shell_workdir_fragment = true;
                true
            }
        });
        if let Some(role) = self.config.available_roles.get(role_name) {
            fragments.extend(
                role.prompt_fragments
                    .iter()
                    .map(|fragment| SourcedPromptFragment {
                        source: PromptFragmentSource::RoleConfig {
                            role_name: role_name.to_owned(),
                        },
                        fragment: PromptFragment::new(
                            fragment.name.clone(),
                            fragment.priority,
                            fragment.text.clone(),
                        ),
                    }),
            );
        }
        let enabled_group_keys = providers
            .iter()
            .filter(|provider| provider_enabled(provider))
            .filter_map(|provider| {
                provider
                    .tool_group
                    .as_ref()
                    .map(|group| (provider.connection_id.clone(), group.name.clone()))
            })
            .collect::<HashSet<_>>();
        let mut seen_group_fragments = HashSet::new();
        let mut tool_fragments = Vec::new();
        for provider in providers.iter().copied() {
            let tool_prompt_repeated_by_group = provider
                .tool_group
                .as_ref()
                .and_then(|group| group.prompt_fragment.as_ref())
                .is_some_and(|group_fragment| {
                    provider
                        .prompt_fragment
                        .as_ref()
                        .is_some_and(|tool_fragment| tool_fragment.name == group_fragment.name)
                });
            if !tool_prompt_repeated_by_group
                && provider_enabled(provider)
                && let Some(fragment) = &provider.prompt_fragment
            {
                let visible_name = self.tool_model_visible_name(&provider.tool);
                tool_fragments.push(SourcedToolPromptFragment {
                    source: PromptFragmentSource::Tool {
                        connection_id: provider.connection_id.clone(),
                    },
                    tool_name: visible_name.clone(),
                    fragment: fragment.clone(),
                });
            }
            if let Some(group) = &provider.tool_group
                && let Some(fragment) = &group.prompt_fragment
                && enabled_group_keys
                    .contains(&(provider.connection_id.clone(), group.name.clone()))
                && seen_group_fragments.insert((
                    provider.connection_id.clone(),
                    group.name.clone(),
                    fragment.name.clone(),
                ))
            {
                tool_fragments.push(SourcedToolPromptFragment {
                    source: PromptFragmentSource::Tool {
                        connection_id: provider.connection_id.clone(),
                    },
                    tool_name: ToolName::new(group.name.as_str()),
                    fragment: fragment.clone(),
                });
            }
        }
        (fragments, tool_fragments)
    }

    pub(super) fn tool_definitions_from_specs(
        &self,
        specs: &[tau_proto::ToolSpec],
    ) -> Vec<ToolDefinition> {
        specs
            .iter()
            .map(|spec| ToolDefinition {
                name: spec.name.clone(),
                model_visible_name: spec.model_visible_name.clone(),
                description: spec.description.clone(),
                tool_type: spec.tool_type,
                parameters: spec.parameters.clone(),
                format: spec.format.clone(),
            })
            .collect()
    }

    #[cfg(test)]
    pub(super) fn gather_tool_definitions_for_role(&self, role_name: &str) -> Vec<ToolDefinition> {
        let model = model_for_role(
            &self.provider_runtime.model_info,
            &self.config.available_roles,
            role_name,
        );
        let specs = self.gather_effective_tool_specs_for_role_model(role_name, model.as_ref());
        self.tool_definitions_from_specs(&specs)
    }

    pub(super) fn gather_effective_tool_specs_for_role_model(
        &self,
        role_name: &str,
        model: Option<&ModelId>,
    ) -> Vec<tau_proto::ToolSpec> {
        let providers = self.sorted_prompt_tool_providers();
        self.gather_effective_tool_specs_for_role_model_from_providers(role_name, model, &providers)
    }

    /// Resolve effective tool specs from one already-sorted provider snapshot.
    fn gather_effective_tool_specs_for_role_model_from_providers(
        &self,
        role_name: &str,
        model: Option<&ModelId>,
        providers: &[&tau_core::ToolProvider],
    ) -> Vec<tau_proto::ToolSpec> {
        let model_info = model.and_then(|model| self.provider_runtime.model_info.get(model));
        let supported_tool_types = model_info.map(|info| info.supported_tool_types.as_slice());
        let mut specs: Vec<_> = providers
            .iter()
            .copied()
            .filter(|provider| {
                let provider_supports_type = supported_tool_types
                    .is_none_or(|supported| supported.contains(&provider.tool.tool_type));
                let requires_image_content = provider
                    .tool
                    .tags
                    .iter()
                    .any(|tag| tag.as_str() == "provider-content:image");
                let provider_supports_image_content = !requires_image_content
                    || model_info.is_some_and(|info| {
                        info.input_modalities
                            .contains(&tau_proto::InputModality::Image)
                            && info
                                .tool_result_modalities
                                .contains(&tau_proto::InputModality::Image)
                    });
                provider_supports_type
                    && provider_supports_image_content
                    && self.is_tool_enabled_for_role_model(
                        &provider.tool,
                        provider.tool_group.as_ref(),
                        role_name,
                        model,
                    )
            })
            .map(|provider| provider.tool.clone())
            .collect();
        self.decorate_agent_start_descriptions(&mut specs);
        specs
    }

    /// Add currently visible, model-available delegate role names to cloned
    /// `agent_start` specs in an effective provider-facing snapshot.
    pub(super) fn decorate_agent_start_descriptions(&self, specs: &mut [tau_proto::ToolSpec]) {
        let role_names = self.visible_available_delegate_role_names();
        if role_names.is_empty() {
            return;
        }
        let suffix = format!(". Roles: {}", role_names.join(", "));
        for spec in specs
            .iter_mut()
            .filter(|spec| spec.name.as_str() == "agent_start")
        {
            if let Some(description) = &mut spec.description {
                description.push_str(&suffix);
            }
        }
    }

    pub(super) fn tool_model_visible_name<'a>(
        &self,
        spec: &'a tau_proto::ToolSpec,
    ) -> &'a ToolName {
        spec.model_visible_name.as_ref().unwrap_or(&spec.name)
    }

    pub(super) fn has_registered_tool_name(&self, requested_name: &ToolName) -> bool {
        for spec in self.tool_routing.registry.all_tools() {
            if spec.name == *requested_name || self.tool_model_visible_name(spec) == requested_name
            {
                return true;
            }
        }
        false
    }

    pub(super) fn nearest_enabled_tool_name_for_role(
        &self,
        requested_name: &ToolName,
        role_name: &str,
    ) -> Option<String> {
        let names = self
            .tool_routing
            .registry
            .all_tool_providers()
            .into_iter()
            .filter(|provider| self.is_tool_provider_enabled_for_role(provider, role_name))
            .map(|provider| self.tool_model_visible_name(&provider.tool).as_str());
        nearest_name_suggestion(requested_name.as_str(), names)
    }

    pub(super) fn nearest_enabled_tool_name_for_prompt(
        &self,
        requested_name: &ToolName,
        agent_prompt_id: &AgentPromptId,
    ) -> Option<String> {
        // Unavailable-tool diagnostics for model calls must be based on the
        // exact prompt-owned tool snapshot when one exists. The role's live tool
        // surface may have changed since the provider saw the prompt; suggesting
        // a current-role-only tool would steer the model toward a tool it could
        // not have selected in that turn.
        let specs = self
            .prompt_coordination
            .prompt_runtime
            .tool_specs
            .get(agent_prompt_id)?;
        let names = specs
            .iter()
            .map(|spec| self.tool_model_visible_name(spec).as_str());
        nearest_name_suggestion(requested_name.as_str(), names)
    }

    pub(super) fn tool_call_waits_for_staged_registration(
        &self,
        cid: &AgentId,
        requested_name: &ToolName,
        agent_prompt_id: Option<&AgentPromptId>,
    ) -> bool {
        let Some((internal_name, visible_name)) =
            self.staged_wait_tool_names(cid, requested_name, agent_prompt_id)
        else {
            return false;
        };
        self.extensions.activation_staging.values().any(|stage| {
            stage.tool_registrations.iter().any(|registration| {
                registration.tool.name == internal_name
                    || self.tool_model_visible_name(&registration.tool) == &visible_name
            })
        })
    }

    pub(super) fn staged_wait_tool_names(
        &self,
        cid: &AgentId,
        requested_name: &ToolName,
        agent_prompt_id: Option<&AgentPromptId>,
    ) -> Option<(ToolName, ToolName)> {
        if let Some(agent_prompt_id) = agent_prompt_id {
            let spec =
                self.resolve_enabled_tool_spec_for_prompt(requested_name, agent_prompt_id)?;
            if self
                .tool_routing
                .registry
                .resolve_provider(&spec.name)
                .is_some()
            {
                return None;
            }
            return Some((
                spec.name.clone(),
                self.tool_model_visible_name(spec).clone(),
            ));
        }

        let role_name = self.role_name_for_agent_id(cid);
        if self
            .resolve_enabled_tool_name_for_role(requested_name, &role_name)
            .is_some()
        {
            return None;
        }
        self.extensions
            .activation_staging
            .values()
            .flat_map(|stage| stage.tool_registrations.iter())
            .find(|registration| {
                self.is_registered_tool_enabled_for_role(registration, &role_name)
                    && (registration.tool.name == *requested_name
                        || self.tool_model_visible_name(&registration.tool) == requested_name)
            })
            .map(|registration| {
                (
                    registration.tool.name.clone(),
                    self.tool_model_visible_name(&registration.tool).clone(),
                )
            })
    }

    pub(super) fn is_tool_enabled_for_role_model(
        &self,
        spec: &tau_proto::ToolSpec,
        group: Option<&tau_proto::ToolGroup>,
        role_name: &str,
        model: Option<&ModelId>,
    ) -> bool {
        let mut enabled = spec.enabled_by_default;
        let model_tags = model
            .and_then(|model| self.provider_runtime.model_info.get(model))
            .map(|info| info.tags.as_slice())
            .unwrap_or(&[]);
        match self.shell_tool_style_for_base_enablement(model_tags) {
            Some(ShellToolStyle::Codex)
                if spec
                    .tags
                    .iter()
                    .any(|tag| tag.as_str().starts_with("shell:")) =>
            {
                enabled = spec.tags.iter().any(|tag| {
                    matches!(
                        tag.as_str(),
                        "shell:edit:apply_patch"
                            | "shell:read:image"
                            | "shell:exec:shell_command"
                            | "shell:workdir"
                            | "shell:lock"
                    )
                });
            }
            Some(style) if spec.tags.iter().any(|tag| tag.as_str() == "shell:edit") => {
                enabled = spec.tags.iter().any(|tag| {
                    tag.as_str()
                        == match style {
                            ShellToolStyle::Edit => "shell:edit:line",
                            ShellToolStyle::Replace => "shell:edit:replace",
                            ShellToolStyle::Codex => unreachable!("handled above"),
                        }
                });
            }
            Some(_) => {}
            None if self.shell_tool_style(model_tags).is_none()
                && spec.tags.iter().any(|tag| tag.as_str() == "shell:edit") =>
            {
                enabled = false;
            }
            None => {}
        }
        let mut rules: Vec<_> = self.config.tool_policy.rules.iter().collect();
        rules.sort_by(|(left_name, left), (right_name, right)| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left_name.cmp(right_name))
        });
        for (_, rule) in rules {
            if !(!rule.enable
                || !rule.when.model_tags.iter().all(|pattern| {
                    model_tags
                        .iter()
                        .any(|model_tag| pattern.matches(model_tag))
                }))
            {
                if tags_match_any(&spec.tags, &rule.disable_tool_tags) {
                    enabled = false;
                }
                if tags_match_any(&spec.tags, &rule.enable_tool_tags) {
                    enabled = true;
                }
            }
        }

        let Some(role) = self.config.available_roles.get(role_name) else {
            return enabled;
        };
        if let Some(tools) = &role.tools {
            enabled = tools.iter().any(|name| name == &spec.name);
        }
        if tags_match_any(&spec.tags, &role.disable_tool_tags) {
            enabled = false;
        }
        if tags_match_any(&spec.tags, &role.enable_tool_tags) {
            enabled = true;
        }
        if let Some(group) = group {
            if role
                .disable_tool_groups
                .iter()
                .any(|name| name == &group.name)
            {
                enabled = false;
            }
            if role
                .enable_tool_groups
                .iter()
                .any(|name| name == &group.name)
            {
                enabled = true;
            }
        }
        if role.disable_tools.iter().any(|name| name == &spec.name) {
            enabled = false;
        }
        if role.enable_tools.iter().any(|name| name == &spec.name) {
            enabled = true;
        }
        enabled
    }

    /// Resolves the requested shell surface before ordinary policy and role
    /// controls.
    pub(super) fn shell_tool_style(
        &self,
        model_tags: &[tau_proto::ModelTag],
    ) -> Option<ShellToolStyle> {
        if let Some(style) = self.config.tool_policy.default_shell_tool_style {
            return Some(style);
        }
        let explicit: HashSet<_> = model_tags
            .iter()
            .filter_map(|tag| match tag.as_str() {
                "shell:tool-style:codex" => Some(ShellToolStyle::Codex),
                "shell:tool-style:edit" => Some(ShellToolStyle::Edit),
                "shell:tool-style:replace" => Some(ShellToolStyle::Replace),
                _ => None,
            })
            .collect();
        match explicit.len() {
            0 => {
                if model_tags.iter().any(|tag| tag.as_str() == "shell:chatgpt") {
                    Some(ShellToolStyle::Codex)
                } else {
                    Some(ShellToolStyle::Replace)
                }
            }
            1 => explicit.iter().copied().next(),
            _ => None,
        }
    }

    /// Leaves legacy ChatGPT/Codex models to their existing configurable policy
    /// rule, preserving the documented escape hatch that disables that bundle.
    pub(super) fn shell_tool_style_for_base_enablement(
        &self,
        model_tags: &[tau_proto::ModelTag],
    ) -> Option<ShellToolStyle> {
        (self.config.tool_policy.default_shell_tool_style.is_some()
            || model_tags.iter().any(|tag| {
                matches!(
                    tag.as_str(),
                    "shell:tool-style:codex" | "shell:tool-style:edit" | "shell:tool-style:replace"
                )
            })
            || !model_tags.iter().any(|tag| tag.as_str() == "shell:chatgpt"))
        .then(|| self.shell_tool_style(model_tags))
        .flatten()
    }

    /// Returns a prompt error for invalid style metadata or unavailable forced
    /// Codex support.
    pub(super) fn shell_tool_style_error(&self, model: Option<&ModelId>) -> Option<String> {
        let info = model.and_then(|id| self.provider_runtime.model_info.get(id))?;
        let explicit: HashSet<_> = info
            .tags
            .iter()
            .filter_map(|tag| match tag.as_str() {
                "shell:tool-style:codex" => Some("codex"),
                "shell:tool-style:edit" => Some("edit"),
                "shell:tool-style:replace" => Some("replace"),
                _ => None,
            })
            .collect();
        if 1 < explicit.len() {
            return Some("conflicting shell tool style tags".to_owned());
        }
        (self.codex_style_is_forced(&info.tags)
            && !info
                .supported_tool_types
                .contains(&tau_proto::ToolType::Custom))
        .then_some("Codex shell tool style requires Custom tool support".to_owned())
    }

    /// Returns whether config or an explicit model style tag, rather than the
    /// legacy ChatGPT default, required the Custom/Text Codex surface.
    pub(super) fn codex_style_is_forced(&self, model_tags: &[tau_proto::ModelTag]) -> bool {
        self.config.tool_policy.default_shell_tool_style == Some(ShellToolStyle::Codex)
            || model_tags
                .iter()
                .any(|tag| tag.as_str() == "shell:tool-style:codex")
    }

    pub(super) fn resolve_enabled_tool_spec_for_role(
        &self,
        requested_name: &ToolName,
        role_name: &str,
    ) -> Option<&tau_proto::ToolSpec> {
        for provider in self.tool_routing.registry.all_tool_providers() {
            let spec = &provider.tool;
            if !self.is_tool_provider_enabled_for_role(provider, role_name) {
                continue;
            }
            if self.tool_model_visible_name(spec) == requested_name {
                return Some(spec);
            }
        }
        None
    }

    pub(super) fn resolve_enabled_tool_spec_for_prompt(
        &self,
        requested_name: &ToolName,
        agent_prompt_id: &AgentPromptId,
    ) -> Option<&tau_proto::ToolSpec> {
        let specs = self
            .prompt_coordination
            .prompt_runtime
            .tool_specs
            .get(agent_prompt_id)?;
        specs
            .iter()
            .find(|spec| self.tool_model_visible_name(spec) == requested_name)
    }

    pub(super) fn resolve_enabled_tool_name_for_role(
        &self,
        requested_name: &ToolName,
        role_name: &str,
    ) -> Option<(ToolName, ToolName)> {
        self.resolve_enabled_tool_spec_for_role(requested_name, role_name)
            .map(|spec| {
                (
                    spec.name.clone(),
                    self.tool_model_visible_name(spec).clone(),
                )
            })
    }

    pub(super) fn is_registered_tool_enabled_for_role(
        &self,
        registration: &ToolRegistrationDeclared,
        role_name: &str,
    ) -> bool {
        self.is_tool_enabled_for_role(
            &registration.tool,
            registration.tool_group.as_ref(),
            role_name,
        )
    }

    pub(super) fn is_tool_provider_enabled_for_role(
        &self,
        provider: &tau_core::ToolProvider,
        role_name: &str,
    ) -> bool {
        self.is_tool_enabled_for_role(&provider.tool, provider.tool_group.as_ref(), role_name)
    }

    pub(super) fn is_tool_enabled_for_role(
        &self,
        spec: &tau_proto::ToolSpec,
        group: Option<&tau_proto::ToolGroup>,
        role_name: &str,
    ) -> bool {
        self.is_tool_enabled_for_role_model(
            spec,
            group,
            role_name,
            self.config.selected_model.as_ref(),
        )
    }
}
