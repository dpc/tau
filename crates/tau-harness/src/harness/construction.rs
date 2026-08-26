//! Harness startup inputs, store opening, role loading, and base assembly.
//!
//! This module constructs runtime state without changing the event sequencing
//! owned by [`Harness`]. Persistence and extension-interface semantics remain
//! governed by
//! [GATE-persistence-and-extension-interface-change-approval](../../../../
//! specs/GATE-persistence-and-extension-interface-change-approval.md).

use super::*;

/// Immutable construction inputs shared across configured harness startup
/// stages.
struct HarnessConstructionInputs {
    /// Initial session reason and persistence policy.
    launch: HarnessSessionLaunch,
    /// Whether startup suppresses environment-backed secret sources.
    ignore_secret_source_environment: bool,
    /// Whether the agent store is process-local independently of extension
    /// storage.
    memory_only_agent_store: bool,
    /// Absolute canonical project root captured for this harness startup.
    project_root: PathBuf,
}
/// One in-process tool extension to spawn alongside the echo provider during
/// tests.
#[cfg(any(test, feature = "echo-agent"))]
pub(crate) struct InProcessTool {
    pub(crate) name: &'static str,
    pub(crate) runner: fn(UnixStream, UnixStream, PathBuf) -> Result<(), String>,
}
struct HarnessBaseParts {
    /// Sender side of the harness event channel.
    tx: Sender<HarnessEvent>,
    /// Receiver side of the harness event channel.
    rx: Receiver<HarnessEvent>,
    /// Producer side of the bounded component-ingress lane.
    component_ingress_tx: ComponentIngressSender,
    /// Harness-owned bounded component-ingress receiver.
    component_ingress: ComponentIngress,
    /// Event bus for live connections and subscriptions.
    bus: EventBus,
    /// Runtime state directory for this harness.
    state_dir: PathBuf,
    /// Complete accepted startup settings retained as the runtime baseline.
    harness_settings: tau_config::settings::HarnessSettings,
    /// Session membership store, with the eager session already loaded.
    store: SessionStore,
    /// Per-agent transcript store.
    agent_store: AgentStore,
    /// Immutable policy for semantic stores, diagnostics, retention, and
    /// delegated extension storage.
    storage_mode: crate::HarnessStorageMode,
    /// Absolute canonical startup root for this harness.
    project_root: PathBuf,
    /// Session id the harness is initially bound to.
    current_session_id: SessionId,
    /// Reason associated with the initial session binding.
    current_session_start_reason: tau_proto::SessionStartReason,
    /// Roles available after applying harness settings.
    available_roles: HashMap<String, tau_config::settings::AgentRole>,
    /// Role groups available for navigation and UI display.
    available_role_groups: Vec<tau_proto::HarnessRoleGroup>,
    /// Receiver-capable roles in deterministic configured order.
    inter_session_receivers: Vec<crate::model::InterSessionReceiverRole>,
    /// Reusable prompt templates loaded from effective harness settings.
    custom_prompts: Vec<tau_proto::HarnessCustomPrompt>,
    /// Runtime role overrides loaded from settings.
    role_overrides: HashMap<String, tau_config::settings::AgentRole>,
    /// Harness-owned declarative tool tag policy.
    tool_policy: tau_config::settings::ToolPolicy,
    /// Inclusive effective bounds for activating-input `wait` calls.
    input_wait_timeout_bounds: (u64, u64),
    /// Approved disabled-by-default Provider cache refresh policy.
    provider_cache_refresh: tau_config::settings::ProviderCacheRefresh,
    /// Initially selected role name.
    selected_role: String,
    /// Initially selected model, if any provider metadata can resolve one.
    selected_model: Option<ModelId>,
    /// Template used to mint new agent ids.
    agent_id_template: String,
    /// Template used to display newly created agents.
    agent_display_name_template: Option<String>,
    /// Loaded system prompt templates keyed by template name.
    system_prompt_templates: HashMap<String, String>,
}
struct ConfiguredHarnessStartup {
    /// Session root used for stores, debug logs, and supervised extension logs.
    sessions_dir: PathBuf,
    /// Secrets and optional-extension skip state resolved before extension
    /// spawn.
    extension_secrets: ResolvedExtensionSecrets,
    /// Missing configured startup role warning to surface after clients attach.
    missing_default_role: Option<MissingDefaultRole>,
    /// Timestamp used for startup tracing.
    started_at: Instant,
}
struct StartupRoles {
    /// Effective roles loaded from harness settings.
    available_roles: HashMap<String, tau_config::settings::AgentRole>,
    /// Runtime role overrides loaded from settings.
    role_overrides: HashMap<String, tau_config::settings::AgentRole>,
    /// Startup role selected after fallback handling.
    selected_role: String,
    /// Role groups visible to clients.
    available_role_groups: Vec<tau_proto::HarnessRoleGroup>,
    /// Receiver-capable roles in deterministic configured order.
    inter_session_receivers: Vec<crate::model::InterSessionReceiverRole>,
    /// Warning emitted when the configured default role was unavailable.
    missing_default_role: Option<MissingDefaultRole>,
    /// Model selected for the startup role before provider metadata arrives.
    selected_model: Option<ModelId>,
}
struct StartupHarnessParts {
    /// Runtime state directory for this harness.
    state_dir: PathBuf,
    /// Filesystem/config directories used by the harness.
    dirs: tau_config::settings::TauDirs,
    /// Session store with the eager session not yet loaded.
    store: SessionStore,
    /// Per-agent transcript store.
    agent_store: AgentStore,
    /// Session id the harness is initially bound to.
    eager_session_id: String,
    /// Launch reason and persistence policy.
    launch: HarnessSessionLaunch,
    /// Effective harness settings.
    harness_settings: tau_config::settings::HarnessSettings,
    /// Startup role selection state.
    roles: StartupRoles,
    /// Absolute canonical project root captured for this harness startup.
    project_root: PathBuf,
}

impl Harness {
    /// Enables the test-only echo tool explicitly for every configured role.
    #[cfg(any(test, feature = "echo-agent"))]
    pub(crate) fn enable_echo_tool_for_tests(&mut self) {
        let echo = tau_proto::ToolName::new("echo");
        for role in self.available_roles.values_mut() {
            if !role.enable_tools.iter().any(|tool| tool == &echo) {
                role.enable_tools.push(echo.clone());
            }
        }
    }
    fn from_base_parts(parts: HarnessBaseParts) -> Self {
        let discovered_skills = built_in_discovered_skills();
        let initial_effective_skills = effective_skills(&discovered_skills);
        let initial_session_id = parts.current_session_id.clone();
        let discovered_skill_candidates = discovered_skills
            .iter()
            .map(|(name, skill)| (name.clone(), vec![skill.clone()]))
            .collect();
        Self {
            tx: parts.tx,
            rx: parts.rx,
            component_ingress_tx: parts.component_ingress_tx,
            component_ingress: parts.component_ingress,
            pending_runtime_event: None,
            #[cfg(test)]
            runtime_event_receive_cut: None,
            bus: parts.bus,
            registry: ToolRegistry::new(),
            action_registry: ActionRegistry::new(),
            internal_tool_handlers: Vec::new(),
            state_dir: parts.state_dir,
            provider_settings_snapshots: BTreeMap::new(),
            accepted_harness_settings: parts.harness_settings,
            store: parts.store,
            agent_store: parts.agent_store,
            storage_mode: parts.storage_mode,
            runtime_harness_path: None,
            project_root: parts.project_root,
            current_session_id: parts.current_session_id,
            current_session_generation: 0,
            next_agent_runtime_incarnation: 1,
            next_agent_initialization_id: 1,
            accounting_runtime_id: accounting_runtime_id(rand::random::<u64>()),
            current_session_start_reason: parts.current_session_start_reason,
            agent_id_rng: StdRng::from_entropy(),
            tool_runtime: ToolRuntimeState::default(),
            event_log: EventLog::new(),
            creator_topology: AgentCreatorTopology::default(),
            cost_ledger: AgentCostLedger::default(),
            ui_runtime: UiRuntimeState::default(),
            external_message_peers: HashSet::new(),
            pending_external_message_auth: HashMap::new(),
            pending_external_receive_acks: HashMap::new(),
            peer_input_rate: HashMap::new(),
            uncommitted_peer_auto_starts: HashSet::new(),
            peer_io_cancellations: Vec::new(),
            inbound_peer_io_cancellations: HashMap::new(),
            lifecycle_messages: Vec::new(),
            replayable_harness_notices: Vec::new(),
            extensions: ExtensionRuntimeState::default(),
            initial_extension_tool_preflight_complete: false,
            resolving_initial_extension_collisions: false,
            next_deferred_extension_message_order: 0,
            enabled_extension_names: BTreeSet::new(),
            prompt_agents: HashMap::new(),
            ephemeral_provider_prompts: HashSet::new(),
            ephemeral_provider_retry_requests: HashSet::new(),
            derived_publish_source: None,
            agents: HashMap::new(),
            agent_routes: HashMap::new(),
            user_interaction_order: HashMap::new(),
            next_user_interaction_order: 1,
            precommitted_agent_starts: HashSet::new(),
            precommitted_user_interactions: HashMap::new(),
            session_loaded_agents: HashSet::new(),
            session_ever_loaded_agents: HashSet::new(),
            session_roster_loaded_agents: HashSet::new(),
            session_roster_ever_loaded_agents: HashSet::new(),
            session_roster_valid: true,
            agent_navigation_modes: HashMap::new(),
            agent_watches: HashMap::new(),
            agent_watchers: HashMap::new(),
            agent_watch_subscriptions: HashMap::new(),
            agent_watch_provider_status: HashMap::new(),
            agent_watch_provider_deliveries: HashMap::new(),
            pending_long_wait_notifications: VecDeque::new(),
            long_wait_materialization_budget: None,
            last_live_egress_lag_warning: None,
            stopped_agent_ids: HashSet::new(),
            restored_unavailable_agents: HashMap::new(),
            pending_agent_unload_reasons: HashMap::new(),
            expected_agent_unloads: HashSet::new(),
            pending_watch_retirements: HashMap::new(),
            pending_builtin_delegates: HashMap::new(),
            turn_state: TurnState::Idle,
            session_init_progress_generation: SessionInitProgressGeneration::default(),
            debug_log: None,
            debug_log_poisoned: false,
            interceptors: InterceptorRegistry::default(),
            suspended_interceptor_connections: HashSet::new(),
            pending_intercept: None,
            pending_publish_error: None,
            disconnect_terminal_batch_pending: HashSet::new(),
            disconnect_terminal_batch_completed: Vec::new(),
            deferred_publishes: VecDeque::new(),
            pending_publish_idle_dispatches: VecDeque::new(),
            available_models: Vec::new(),
            provider_models_by_extension: HashMap::new(),
            provider_quota: HashMap::new(),
            provider_quota_capabilities: HashMap::new(),
            provider_quota_tombstones: HashMap::new(),
            provider_quota_retired_epochs: HashMap::new(),
            provider_model_info: HashMap::new(),
            provider_model_routes: HashMap::new(),
            provider_cache_residency: ProviderCacheResidency::runtime(parts.provider_cache_refresh),
            cache_refresh_tool_window_calls: HashSet::new(),
            pending_provider_prompts: HashMap::new(),
            pending_prompt_dispatches: HashSet::new(),
            available_roles: parts.available_roles,
            disabled_role_reasons: HashMap::new(),
            available_role_groups: parts.available_role_groups,
            inter_session_receivers: parts.inter_session_receivers,
            peer_route_clock: 0,
            peer_last_routed: HashMap::new(),
            custom_prompts: parts.custom_prompts,
            role_overrides: parts.role_overrides,
            tool_policy: parts.tool_policy,
            agent_id_template: parts.agent_id_template,
            agent_display_name_template: parts.agent_display_name_template,
            selected_role: parts.selected_role,
            selected_model: parts.selected_model,
            current_session_state: CurrentSessionState::default(),
            prompt_models: HashMap::new(),
            prompt_estimated_cost_rates: HashMap::new(),
            prompt_context_limits: HashMap::new(),
            prompt_context_size_alerts: HashMap::new(),
            prompt_compaction_policies: HashMap::new(),
            prompt_compaction_projected_tokens: HashMap::new(),
            prompt_semantic_output: HashSet::new(),
            pending_stale_provider_responses: HashMap::new(),
            pending_replay_prompt_activation_occurrences: HashMap::new(),
            pending_replay_uncertain_stale: HashMap::new(),
            local_route_failure_prompts: HashSet::new(),
            suppressed_compaction_dispatches: HashSet::new(),
            silent_compaction_failure_prompts: HashSet::new(),
            cancelled_compaction_claims: HashSet::new(),
            pending_manual_compaction_tools: HashMap::new(),
            accepted_manual_compaction_tools: HashMap::new(),
            pending_ui_compactions_after_wait: HashMap::new(),
            enqueued_standalone_inference_checkpoints: HashSet::new(),
            pending_agent_publish_completions: HashMap::new(),
            pending_initial_prompt_correlations: HashMap::new(),
            prompt_operations: HashMap::new(),
            prompt_tool_specs: HashMap::new(),
            prompt_tool_call_prompts: HashMap::new(),
            shown_tool_failure_examples: HashSet::new(),
            discovered_skills,
            discovered_skill_candidates,
            discovered_agents_files: Vec::new(),
            agent_context: AgentContextStore::default(),
            agent_context_providers: HashSet::new(),
            session_context_providers: HashSet::new(),
            pending_agent_discovery: HashMap::new(),
            frozen_agent_discovery: HashMap::new(),
            agent_context_initialized: HashMap::new(),
            pending_rendered_prompts: HashMap::new(),
            session_skills_available: tau_proto::HarnessSessionSkillsAvailable {
                session_id: initial_session_id,
                skills: initial_effective_skills,
            },
            extension_prompt_fragments: BTreeMap::new(),
            system_prompt_templates: parts.system_prompt_templates,
            initialized_sessions: HashSet::new(),
            pending_notices: PendingPromptNoticeState::default(),
            agent_runtime_indicators: HashMap::new(),
            canceled_prompts: HashSet::new(),
            pending_start_agent_requests: VecDeque::new(),
            subagents: SubagentToolState::with_input_wait_timeout_bounds(
                parts.input_wait_timeout_bounds,
            ),
        }
    }
    #[cfg(any(test, feature = "echo-agent"))]
    pub(crate) fn new_with_provider(
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        provider_runner: ProviderRunner,
        tools: Vec<InProcessTool>,
        eager_session_id: &str,
        eager_session_start_reason: tau_proto::SessionStartReason,
        storage_mode: crate::HarnessStorageMode,
    ) -> Result<Self, HarnessError> {
        Self::new_with_provider_and_internal_tools(
            state_dir,
            dirs,
            provider_runner,
            tools,
            TestProviderHarnessStartup {
                session_id: eager_session_id,
                reason: eager_session_start_reason,
                storage_mode,
                internal_tool_handlers: Vec::new(),
            },
        )
    }
    #[cfg(any(test, feature = "echo-agent"))]
    pub(crate) fn new_with_provider_and_internal_tools(
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        provider_runner: ProviderRunner,
        tools: Vec<InProcessTool>,
        startup: TestProviderHarnessStartup<'_>,
    ) -> Result<Self, HarnessError> {
        let TestProviderHarnessStartup {
            session_id: eager_session_id,
            reason: eager_session_start_reason,
            storage_mode,
            internal_tool_handlers,
        } = startup;
        let launch = HarnessSessionLaunch {
            reason: eager_session_start_reason,
            storage_mode,
        }
        .validate()?;
        let storage_mode = launch.storage_mode;
        let state_dir = state_dir.into();
        let harness_settings = crate::settings::load_harness_settings_without_environment(&dirs)
            .map_err(|error| HarnessError::Participant(error.to_string()))?;
        let project_root = if storage_mode.is_memory_only() {
            if state_dir.is_dir() {
                state_dir.canonicalize()?
            } else {
                state_dir
                    .parent()
                    .ok_or_else(|| {
                        HarnessError::Participant("test state directory has no parent".to_owned())
                    })?
                    .canonicalize()?
            }
        } else {
            let project_root = state_dir.join("test-project");
            std::fs::create_dir_all(&project_root)?;
            project_root.canonicalize()?
        };
        let sessions_dir = tau_config::settings::sessions_dir_of(&state_dir);
        let (tx, rx) = mpsc::channel();
        let (component_ingress, component_ingress_tx) =
            ComponentIngress::new(tx.clone(), ComponentIngressCapacity::One);
        let bus = EventBus::new();
        // Lazy: only the eager session's tree is needed up front
        // (loaded below via `store.load_session`); other sessions
        // load on first access. Avoids a startup walk over every
        // historical session dir.
        let agents_dir = state_dir.join("agents");
        let store = if storage_mode.is_ephemeral() {
            SessionStore::open_ephemeral(&sessions_dir)?
        } else {
            SessionStore::open_lazy(&sessions_dir)?
        };
        let agent_store = if storage_mode.is_memory_only() {
            AgentStore::open_memory_only(&agents_dir)
        } else {
            AgentStore::open_lazy(&agents_dir)?
        };

        let own_pid = std::process::id();
        let mut next_iid = instance_id_factory();

        let mut extension_connects = Vec::new();
        // Provider
        let provider_spawn = spawn_in_process(
            "provider",
            ClientKind::Provider,
            provider_runner,
            &tx,
            &component_ingress_tx,
        )?;
        let provider_conn_id = provider_spawn.connection_id.clone();
        extension_connects.push(ExtensionConnectCommand {
            entry: ExtensionEntry {
                name: tau_proto::ExtensionName::parse("provider")
                    .expect("built-in provider name must satisfy the extension identifier grammar"),
                instance_id: next_iid(),
                connection_id: provider_conn_id,
                kind: ClientKind::Provider,
                peer_capabilities: Default::default(),
                tool_prefix: None,
                require: true,
                respawn_allowed: true,
                pid: Some(own_pid),
                in_process_thread: Some(provider_spawn.thread),
                supervised_config: None,
                secrets: BTreeMap::new(),
                restart_attempt: 0,
                state: ExtensionState::Spawning,
                protocol_io: provider_spawn.protocol_io,
            },
            origin: ConnectionOrigin::Supervised,
            writer_tx: provider_spawn.writer_tx,
            initialized_ack: provider_spawn.initialized_ack,
            supervised_writer: None,
            replaces: None,
        });

        // Caller-supplied in-process tools.
        for tool in tools {
            let project_root = project_root.clone();
            let tool_spawn = spawn_in_process(
                tool.name,
                ClientKind::Tool,
                move |reader, writer| (tool.runner)(reader, writer, project_root),
                &tx,
                &component_ingress_tx,
            )?;
            let conn_id = tool_spawn.connection_id.clone();
            extension_connects.push(ExtensionConnectCommand {
                entry: ExtensionEntry {
                    name: tau_proto::ExtensionName::parse(tool.name)
                        .expect("built-in tool name must satisfy the extension identifier grammar"),
                    instance_id: next_iid(),
                    connection_id: conn_id,
                    kind: ClientKind::Tool,
                    peer_capabilities: Default::default(),
                    tool_prefix: None,
                    require: true,
                    respawn_allowed: true,
                    pid: Some(own_pid),
                    in_process_thread: Some(tool_spawn.thread),
                    supervised_config: None,
                    secrets: BTreeMap::new(),
                    restart_attempt: 0,
                    state: ExtensionState::Spawning,
                    protocol_io: tool_spawn.protocol_io,
                },
                origin: ConnectionOrigin::Supervised,
                writer_tx: tool_spawn.writer_tx,
                initialized_ack: tool_spawn.initialized_ack,
                supervised_writer: None,
                replaces: None,
            });
        }

        let system_prompt_templates = load_system_prompt_templates(dirs.config_dir.as_deref());
        let LoadedRoles {
            roles: available_roles,
            role_overrides,
            selected_role,
            role_groups: available_role_groups,
            inter_session_receivers,
            missing_default_role,
        } = load_roles(&harness_settings);
        let custom_prompts = harness_settings
            .custom_prompts
            .iter()
            .map(|prompt| tau_proto::HarnessCustomPrompt {
                id: prompt.id.clone(),
                text: prompt.text.clone(),
            })
            .collect();
        if available_roles.is_empty() {
            return Err(HarnessError::Participant(
                "no roles are enabled; enable at least one role in harness.yaml or with --enable-role <role>".to_owned(),
            ));
        }
        let selected_model =
            select_model_for_role(&HashMap::new(), &available_roles, &selected_role);
        let mut store = store;
        if matches!(launch.reason, tau_proto::SessionStartReason::Resume) {
            let _ = store.lock_and_load_existing_session(eager_session_id)?;
            Self::create_resumed_harness_log_after_lock(
                &sessions_dir,
                eager_session_id,
                storage_mode,
            )?;
        } else {
            let _ = store.lock_and_load_session(eager_session_id)?;
        }
        if storage_mode.is_durable() {
            // Commit canonical existence before creating any session-owned
            // diagnostic artifact. The lock and directory remain scaffolding
            // until this manifest replacement succeeds.
            store.record_session_meta(eager_session_id)?;
        }
        if storage_mode.is_durable() {
            crate::session_cleanup::spawn_session_cleanup(
                sessions_dir.clone(),
                harness_settings.session_retention(),
                vec![
                    SessionId::parse(eager_session_id).expect("known-safe SessionId must be valid"),
                ],
            );
        }
        crate::diagnostic_cleanup::spawn_diagnostic_cleanup(
            sessions_dir.clone(),
            harness_settings.diagnostic_retention(),
            storage_mode.session_persistence(),
            vec![SessionId::parse(eager_session_id).expect("known-safe SessionId must be valid")],
        );
        let mut harness = Self::from_base_parts(HarnessBaseParts {
            tx,
            rx,
            component_ingress_tx,
            component_ingress,
            bus,
            state_dir: state_dir.clone(),
            harness_settings: harness_settings.clone(),
            store,
            agent_store,
            storage_mode,
            project_root,
            current_session_id: eager_session_id
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            current_session_start_reason: launch.reason,
            available_roles,
            available_role_groups,
            inter_session_receivers,
            custom_prompts,
            role_overrides,
            tool_policy: harness_settings.tool_policy.clone(),
            input_wait_timeout_bounds: harness_settings.wait_timeout_bounds(),
            provider_cache_refresh: harness_settings.provider_cache_refresh,
            selected_role,
            selected_model,
            agent_id_template: harness_settings.agent_id_template.clone(),
            agent_display_name_template: harness_settings.agent_display_name_template.clone(),
            system_prompt_templates,
        });

        if storage_mode.is_durable() {
            // Debug log lives next to the eager-init session's events file
            // so the session dir stays self-contained: `events.cbor` +
            // `events.jsonl` + `meta.json` + `lock`.
            let _ = harness.enable_debug_log(&sessions_dir.join(eager_session_id))?;
        }

        harness.install_internal_tool_handlers(internal_tool_handlers);
        if matches!(launch.reason, tau_proto::SessionStartReason::Resume) {
            harness.rehydrate_agents_from_session();
        }
        harness.publish_current_session_dir();

        for command in extension_connects {
            harness.queue_extension_connect(command)?;
        }
        harness.wait_for_extensions_ready()?;
        #[cfg(test)]
        harness.register_harness_tools();
        harness.publish_delegate_roles_context();
        harness.check_config_exists();
        harness.emit_missing_default_role(missing_default_role);

        // Eager session init for the default session. INTENTIONAL —
        // do NOT "simplify" this to lazy-on-first-prompt.
        //
        // Reasons this is a design choice, not dead weight:
        //
        // 1. **Pre-warm AGENTS.md and skill discovery.** The default session is the
        //    fallback when a caller (embedded or socket) doesn't specify one, and even
        //    when callers pick their own `chat-<ts>` id they still benefit: ext-shell
        //    has already walked the user agent roots + the cwd ancestor chain once, so
        //    the second init is cache-warm.
        //
        // 2. **Surface discovery before the first prompt.** The CLI prints "loaded: …"
        //    as events arrive; doing this at startup gives the user visible
        //    confirmation that their AGENTS.md was found — before they type anything —
        //    instead of bundling that feedback into the first agent response.
        //
        // 3. **Fail loudly at startup, not mid-first-turn.** If a provider hangs or the
        //    discovery logic panics, the process hits `SessionInitTimeout` here rather
        //    than appearing to accept the first prompt and then silently stalling.
        //
        // Every past agent that touched this code has "noticed" that
        // the CLI uses `chat-<ts>` session ids and concluded the eager
        // init is wasted work. It isn't. Please resist the urge.
        harness.start_session_init(
            eager_session_id
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            launch.reason,
        );
        harness.wait_for_session_init()?;
        harness.activate_replayed_prompt_occurrences();
        harness.ensure_selected_role_available_after_required_skill_validation()?;
        Ok(harness)
    }
    /// Creates a harness from configuration, spawning real child processes.
    pub(crate) fn from_config(
        config: &Config,
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        eager_session_id: &str,
        eager_session_start_reason: tau_proto::SessionStartReason,
        storage_mode: crate::HarnessStorageMode,
    ) -> Result<Self, HarnessError> {
        let mut initial_client_error_stream = None;
        Self::from_config_with_initial_client(
            config,
            state_dir,
            dirs,
            eager_session_id,
            HarnessSessionLaunch {
                reason: eager_session_start_reason,
                storage_mode,
            },
            HarnessStartupInputs {
                initial_client: None,
                internal_tool_handlers: Vec::new(),
                ignore_startup_environment: false,
                memory_only_agent_store: false,
                project_root: std::env::current_dir()?.canonicalize()?,
            },
            &mut initial_client_error_stream,
        )
        .map(|(harness, _)| harness)
    }
    /// Creates a harness from configuration resolved without startup
    /// environment transports and suppresses environment-backed secret sources.
    pub(crate) fn from_config_without_startup_environment(
        config: &Config,
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        eager_session_id: &str,
        eager_session_start_reason: tau_proto::SessionStartReason,
        storage_mode: crate::HarnessStorageMode,
    ) -> Result<Self, HarnessError> {
        let mut initial_client_error_stream = None;
        Self::from_config_with_initial_client_policy(
            config,
            state_dir,
            dirs,
            eager_session_id,
            HarnessSessionLaunch {
                reason: eager_session_start_reason,
                storage_mode,
            },
            HarnessStartupInputs {
                initial_client: None,
                internal_tool_handlers: Vec::new(),
                ignore_startup_environment: true,
                memory_only_agent_store: false,
                project_root: std::env::current_dir()?.canonicalize()?,
            },
            &mut initial_client_error_stream,
        )
        .map(|(harness, _)| harness)
    }
    pub(crate) fn from_config_with_initial_client(
        config: &Config,
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        eager_session_id: &str,
        launch: HarnessSessionLaunch,
        startup_inputs: HarnessStartupInputs,
        initial_client_error_stream: &mut Option<InitialClientStartupErrorOutput>,
    ) -> Result<(Self, Option<ConnectionId>), HarnessError> {
        Self::from_config_with_initial_client_policy(
            config,
            state_dir,
            dirs,
            eager_session_id,
            launch,
            startup_inputs,
            initial_client_error_stream,
        )
    }
    fn from_config_with_initial_client_policy(
        config: &Config,
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        eager_session_id: &str,
        launch: HarnessSessionLaunch,
        startup_inputs: HarnessStartupInputs,
        initial_client_error_stream: &mut Option<InitialClientStartupErrorOutput>,
    ) -> Result<(Self, Option<ConnectionId>), HarnessError> {
        let launch = launch.validate()?;
        tracing::debug!(target: "tau_harness::startup", eager_session_id, "constructing harness from config");
        let state_dir = state_dir.into();
        let HarnessStartupInputs {
            initial_client,
            internal_tool_handlers,
            ignore_startup_environment,
            memory_only_agent_store,
            project_root,
        } = startup_inputs;
        let (mut harness, startup) = Self::build_configured_harness(
            config,
            state_dir,
            dirs,
            eager_session_id,
            HarnessConstructionInputs {
                launch,
                ignore_secret_source_environment: ignore_startup_environment,
                memory_only_agent_store,
                project_root,
            },
        )?;
        harness.install_internal_tool_handlers(internal_tool_handlers);

        if matches!(launch.reason, tau_proto::SessionStartReason::Resume) {
            harness.rehydrate_agents_from_session();
        }
        let initial_client_id =
            harness.accept_initial_client(initial_client, initial_client_error_stream)?;
        harness.publish_current_session_dir();
        harness.emit_extension_startup_diagnostics(&config.extension_startup_diagnostics);
        harness.emit_extension_startup_diagnostics(&startup.extension_secrets.diagnostics);

        if let Err(error) = harness.spawn_configured_extensions(
            config,
            &startup.sessions_dir,
            eager_session_id,
            &startup.extension_secrets.secrets,
            &startup.extension_secrets.skipped_extensions,
            startup.started_at,
        ) {
            harness.send_startup_disconnect_to_initial_client(initial_client_id.as_ref(), &error);
            return Err(error);
        }
        if let Err(error) = harness.wait_for_extensions_ready() {
            harness.send_startup_disconnect_to_initial_client(initial_client_id.as_ref(), &error);
            return Err(error);
        }
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup.started_at.elapsed().as_millis(), "extensions ready");
        #[cfg(test)]
        harness.register_harness_tools();
        harness.publish_delegate_roles_context();
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup.started_at.elapsed().as_millis(), "harness tools registered");
        harness.check_config_exists();
        harness.emit_missing_default_role(startup.missing_default_role);
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup.started_at.elapsed().as_millis(), "config checks complete");

        harness.start_session_init(
            eager_session_id
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            launch.reason,
        );
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup.started_at.elapsed().as_millis(), "session init started");
        if let Err(error) = harness.wait_for_session_init() {
            harness.send_startup_disconnect_to_initial_client(initial_client_id.as_ref(), &error);
            return Err(error);
        }
        if let Err(error) = harness.ensure_selected_role_available_after_required_skill_validation()
        {
            harness.send_startup_disconnect_to_initial_client(initial_client_id.as_ref(), &error);
            return Err(error);
        }
        harness.activate_replayed_prompt_occurrences();
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup.started_at.elapsed().as_millis(), "session init complete");
        Ok((harness, initial_client_id))
    }
    fn build_configured_harness(
        config: &Config,
        state_dir: PathBuf,
        dirs: tau_config::settings::TauDirs,
        eager_session_id: &str,
        construction: HarnessConstructionInputs,
    ) -> Result<(Self, ConfiguredHarnessStartup), HarnessError> {
        let startup_started_at = Instant::now();
        let sessions_dir = tau_config::settings::sessions_dir_of(&state_dir);
        let (harness, missing_default_role, extension_secrets) = Self::open_configured_harness(
            config,
            state_dir,
            sessions_dir.clone(),
            dirs,
            eager_session_id,
            construction,
        )?;
        Ok((
            harness,
            ConfiguredHarnessStartup {
                sessions_dir,
                extension_secrets,
                missing_default_role,
                started_at: startup_started_at,
            },
        ))
    }
    pub(super) fn resolve_startup_extension_secrets(
        config: &Config,
        state_dir: &Path,
        secret_sources: &SecretSources,
        provider_bound_names: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<ResolvedExtensionSecrets, HarnessError> {
        resolve_extension_secrets_excluding(config, state_dir, secret_sources, provider_bound_names)
            .map_err(Into::into)
    }
    fn open_configured_harness(
        config: &Config,
        state_dir: PathBuf,
        sessions_dir: PathBuf,
        dirs: tau_config::settings::TauDirs,
        eager_session_id: &str,
        construction: HarnessConstructionInputs,
    ) -> Result<(Self, Option<MissingDefaultRole>, ResolvedExtensionSecrets), HarnessError> {
        let HarnessConstructionInputs {
            launch,
            ignore_secret_source_environment,
            memory_only_agent_store,
            project_root,
        } = construction;
        let startup_started_at = Instant::now();
        if memory_only_agent_store && !launch.storage_mode.is_memory_only() {
            std::fs::create_dir_all(&state_dir)?;
        }
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "opening session store");
        let (store, agent_store) = Self::open_startup_stores(
            &state_dir,
            &sessions_dir,
            launch.storage_mode,
            memory_only_agent_store,
        )?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session store opened");
        let secret_sources = if ignore_secret_source_environment {
            Default::default()
        } else {
            load_secret_sources()?
        };
        let ProviderStartupSnapshot {
            settings: provider_settings_snapshots,
            bound_names: provider_bound_names,
            diagnostics: provider_diagnostics,
            skipped_extensions: provider_skipped_extensions,
        } = if launch.storage_mode.is_memory_only() {
            provider_startup::snapshot_memory_only_provider_settings(
                config,
                dirs.config_dir.as_deref(),
                &state_dir,
            )?
        } else {
            provider_startup::snapshot_and_materialize_named_provider_credentials(
                config,
                dirs.config_dir.as_deref(),
                &state_dir,
                &secret_sources,
            )?
        };
        let mut extension_secrets = Self::resolve_startup_extension_secrets(
            config,
            &state_dir,
            &secret_sources,
            &provider_bound_names,
        )?;
        extension_secrets.diagnostics.extend(provider_diagnostics);
        extension_secrets
            .skipped_extensions
            .extend(provider_skipped_extensions);
        let harness_settings = config.harness_settings.clone();
        let roles = Self::load_startup_roles(&harness_settings)?;
        let missing_default_role = roles.missing_default_role.clone();
        let session_retention = harness_settings.session_retention();
        let diagnostic_retention = harness_settings.diagnostic_retention();
        let storage_mode = launch.storage_mode;
        tracing::debug!(target: "tau_harness::startup", selected_model = ?roles.selected_model, elapsed_ms = startup_started_at.elapsed().as_millis(), "harness settings loaded");
        let mut harness = Self::assemble_startup_harness(StartupHarnessParts {
            state_dir,
            dirs,
            store,
            agent_store,
            eager_session_id: eager_session_id.to_owned(),
            launch,
            harness_settings,
            roles,
            project_root,
        })?;
        harness.provider_settings_snapshots = provider_settings_snapshots;
        harness.enabled_extension_names = config
            .extensions
            .keys()
            .cloned()
            .chain(
                config
                    .extension_startup_diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.extension.clone()),
            )
            .collect();
        harness.prepare_initial_session_storage(
            &sessions_dir,
            eager_session_id,
            startup_started_at,
        )?;
        if launch.storage_mode.is_durable() {
            crate::session_cleanup::spawn_session_cleanup(
                sessions_dir.clone(),
                session_retention,
                vec![
                    SessionId::parse(eager_session_id).expect("known-safe SessionId must be valid"),
                ],
            );
        }
        crate::diagnostic_cleanup::spawn_diagnostic_cleanup(
            sessions_dir.clone(),
            diagnostic_retention,
            storage_mode.session_persistence(),
            vec![SessionId::parse(eager_session_id).expect("known-safe SessionId must be valid")],
        );
        Ok((harness, missing_default_role, extension_secrets))
    }
    fn open_startup_stores(
        state_dir: &Path,
        sessions_dir: &Path,
        storage_mode: crate::HarnessStorageMode,
        memory_only_agent_store: bool,
    ) -> Result<(SessionStore, AgentStore), HarnessError> {
        let store = if storage_mode.is_ephemeral() {
            SessionStore::open_ephemeral(sessions_dir)?
        } else {
            SessionStore::open_lazy(sessions_dir)?
        };
        let agent_store = if storage_mode.is_memory_only() || memory_only_agent_store {
            AgentStore::open_memory_only(state_dir.join("agents"))
        } else {
            AgentStore::open_lazy(state_dir.join("agents"))?
        };
        Ok((store, agent_store))
    }
    fn load_startup_roles(
        harness_settings: &tau_config::settings::HarnessSettings,
    ) -> Result<StartupRoles, HarnessError> {
        let LoadedRoles {
            roles: available_roles,
            role_overrides,
            selected_role,
            role_groups: available_role_groups,
            inter_session_receivers,
            missing_default_role,
        } = load_roles(harness_settings);
        if available_roles.is_empty() {
            return Err(HarnessError::Participant(
                "no roles are enabled; enable at least one role in harness.yaml or with --enable-role <role>".to_owned(),
            ));
        }
        let selected_model =
            select_model_for_role(&HashMap::new(), &available_roles, &selected_role);
        Ok(StartupRoles {
            available_roles,
            role_overrides,
            selected_role,
            available_role_groups,
            inter_session_receivers,
            missing_default_role,
            selected_model,
        })
    }
    fn assemble_startup_harness(mut parts: StartupHarnessParts) -> Result<Self, HarnessError> {
        if matches!(parts.launch.reason, tau_proto::SessionStartReason::Resume) {
            let _ = parts
                .store
                .lock_and_load_existing_session(&parts.eager_session_id)?;
            let sessions_dir = parts.store.sessions_dir().to_path_buf();
            Self::create_resumed_harness_log_after_lock(
                &sessions_dir,
                &parts.eager_session_id,
                parts.launch.storage_mode,
            )?;
        } else {
            let _ = parts.store.lock_and_load_session(&parts.eager_session_id)?;
        }
        let (tx, rx) = mpsc::channel();
        let (component_ingress, component_ingress_tx) =
            ComponentIngress::new(tx.clone(), ComponentIngressCapacity::One);
        let bus = EventBus::new();
        let custom_prompts = parts
            .harness_settings
            .custom_prompts
            .iter()
            .map(|prompt| tau_proto::HarnessCustomPrompt {
                id: prompt.id.clone(),
                text: prompt.text.clone(),
            })
            .collect();
        Ok(Self::from_base_parts(HarnessBaseParts {
            tx,
            rx,
            component_ingress_tx,
            component_ingress,
            bus,
            state_dir: parts.state_dir,
            harness_settings: parts.harness_settings.clone(),
            store: parts.store,
            agent_store: parts.agent_store,
            storage_mode: parts.launch.storage_mode,
            project_root: parts.project_root,
            current_session_id: parts
                .eager_session_id
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            current_session_start_reason: parts.launch.reason,
            available_roles: parts.roles.available_roles,
            available_role_groups: parts.roles.available_role_groups,
            inter_session_receivers: parts.roles.inter_session_receivers,
            custom_prompts,
            role_overrides: parts.roles.role_overrides,
            tool_policy: parts.harness_settings.tool_policy.clone(),
            input_wait_timeout_bounds: parts.harness_settings.wait_timeout_bounds(),
            provider_cache_refresh: parts.harness_settings.provider_cache_refresh,
            selected_role: parts.roles.selected_role,
            selected_model: parts.roles.selected_model,
            agent_id_template: parts.harness_settings.agent_id_template.clone(),
            agent_display_name_template: parts.harness_settings.agent_display_name_template.clone(),
            system_prompt_templates: load_system_prompt_templates(parts.dirs.config_dir.as_deref()),
        }))
    }
    /// Creates the relay target while the session store retains resume
    /// ownership. The parent CLI subsequently opens this file without `create`.
    fn create_resumed_harness_log_after_lock(
        sessions_dir: &Path,
        session_id: &str,
        storage_mode: crate::HarnessStorageMode,
    ) -> Result<(), HarnessError> {
        if !storage_mode.is_durable() {
            return Ok(());
        }
        let harness_log = crate::harness_log_path(sessions_dir, session_id);
        if let Some(parent) = harness_log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        drop(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(harness_log)?,
        );
        Ok(())
    }
    pub(super) fn prepare_initial_session_storage(
        &mut self,
        sessions_dir: &Path,
        eager_session_id: &str,
        startup_started_at: Instant,
    ) -> Result<(), HarnessError> {
        if self.storage_mode.is_ephemeral() {
            return Ok(());
        }
        // Commit canonical existence before creating session-owned diagnostics.
        // The creating lock and directory remain incomplete scaffolding if this
        // replacement fails.
        self.store.record_session_meta(eager_session_id)?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session metadata recorded");
        let _ = self.enable_debug_log(&sessions_dir.join(eager_session_id))?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "debug event log enabled");
        Ok(())
    }
}
