//! Harness startup inputs, store opening, role loading, and base assembly.
//!
//! This module constructs runtime state without changing the event sequencing
//! owned by [`Harness`]. Persistence and extension-interface semantics remain
//! governed by
//! [GATE-persistence-and-extension-interface-change-approval](../../../../
//! specs/GATE-persistence-and-extension-interface-change-approval.md).

use std::sync::Arc;

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
    /// Fully initialized central runtime I/O ownership.
    runtime_io: RuntimeIoState,
    /// Fully initialized session binding and persistence ownership.
    session_runtime: SessionRuntimeState,
    /// Effective startup configuration retained by the harness.
    config: HarnessConfigState,
    /// Inclusive effective bounds for activating-input `wait` calls.
    input_wait_timeout_bounds: tau_config::settings::WaitTimeoutBounds,
    /// Approved disabled-by-default Provider cache refresh policy.
    provider_cache_refresh: tau_config::settings::ProviderCacheRefresh,
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
    /// Unique lifecycle owner injected into both durable stores.
    persistence_owner: Option<Arc<tau_core::SemanticPersistenceOwner>>,
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
        for role in self.config.available_roles.values_mut() {
            if !role.enable_tools.iter().any(|tool| tool == &echo) {
                role.enable_tools.push(echo.clone());
            }
        }
    }
    fn from_base_parts(parts: HarnessBaseParts) -> Self {
        let initial_session_id = parts.session_runtime.current_session_id.clone();
        let harness = Self {
            runtime_io: parts.runtime_io,
            session_runtime: parts.session_runtime,
            config: parts.config,
            tool_routing: ToolRoutingState {
                registry: ToolRegistry::new(),
                action_registry: ActionRegistry::new(),
                internal_tool_handlers: Vec::new(),
                tool_runtime: ToolRuntimeState::default(),
            },
            agent_runtime: AgentRuntimeState {
                agent_registry: AgentRegistryState {
                    agents: HashMap::new(),
                    agent_routes: HashMap::new(),
                    next_runtime_incarnation: 1,
                    next_initialization_id: 1,
                    accounting_runtime_id: accounting_runtime_id(rand::random::<u64>()),
                    id_rng: StdRng::from_entropy(),
                    creator_topology: AgentCreatorTopology::default(),
                    cost_ledger: AgentCostLedger::default(),
                    precommitted_starts: HashSet::new(),
                    session_loaded: HashSet::new(),
                    session_ever_loaded: HashSet::new(),
                    roster_loaded: HashSet::new(),
                    roster_ever_loaded: HashSet::new(),
                    roster_durable_ever_loaded: HashSet::new(),
                    roster_valid: true,
                    navigation_modes: HashMap::new(),
                    stopped_ids: HashSet::new(),
                    restored_unavailable: HashMap::new(),
                    pending_builtin_delegates: HashMap::new(),
                    pending_start_requests: VecDeque::new(),
                    start_coordinator: StartCoordinator::new(),
                },
                agent_watch: AgentWatchState::default(),
                subagents: SubagentToolState::with_input_wait_timeout_bounds(
                    parts.input_wait_timeout_bounds,
                ),
                agent_runtime_indicators: HashMap::new(),
            },
            prompt_coordination: PromptCoordinationState {
                prompt_runtime: PromptRuntimeState::default(),
                compaction_runtime: CompactionRuntimeState::default(),
                standalone_accounting: StandaloneExecutionAccountingState::default(),
                context_discovery: ContextDiscoveryState::new(
                    initial_session_id,
                    parts.system_prompt_templates,
                ),
                pending_notices: PendingPromptNoticeState::default(),
                canceled_prompts: HashSet::new(),
            },
            ui_runtime: UiRuntimeState::default(),
            peer_messaging: PeerMessagingState::default(),
            extensions: ExtensionRuntimeState::default(),
            provider_runtime: ProviderRuntimeState::new(parts.provider_cache_refresh),
        };
        if let Some(owner) = harness.session_runtime.persistence_owner.as_ref() {
            let tx = harness.runtime_io.tx.clone();
            owner.set_operational_wake(Arc::new(move || {
                let _ = tx.send(HarnessEvent::Command(
                    HarnessCommand::SemanticPersistenceProgress,
                ));
            }));
        }
        harness
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
            mode: HarnessSessionLaunchMode::from_reason(eager_session_start_reason),
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
        let persistence_owner = (!storage_mode.is_memory_only())
            .then(|| {
                tau_core::SemanticPersistenceOwner::new(Default::default())
                    .map(Arc::new)
                    .map_err(|error| HarnessError::Participant(error.to_string()))
            })
            .transpose()?;
        let store = if storage_mode.is_ephemeral() {
            SessionStore::open_ephemeral(&sessions_dir)?
        } else {
            SessionStore::open_managed(
                &sessions_dir,
                persistence_owner
                    .as_ref()
                    .expect("durable session has owner")
                    .clone(),
            )?
        };
        let agent_store = if storage_mode.is_memory_only() {
            AgentStore::open_memory_only(&agents_dir)
        } else {
            AgentStore::open_managed(
                &agents_dir,
                persistence_owner
                    .as_ref()
                    .expect("durable agent has owner")
                    .clone(),
            )?
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
        if storage_mode.is_durable() {
            store.prepare_session(eager_session_id, launch.mode.preparation())?;
        } else if launch.mode.is_resume() {
            let _ = store.lock_and_load_existing_session(eager_session_id)?;
        } else {
            let _ = store.lock_and_load_session(eager_session_id)?;
        }
        if launch.mode.is_resume() {
            Self::create_resumed_harness_log_after_lock(
                &sessions_dir,
                eager_session_id,
                storage_mode,
            )?;
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
            runtime_io: RuntimeIoState {
                tx,
                rx,
                component_ingress_tx,
                component_ingress,
                pending_runtime_event: None,
                #[cfg(test)]
                runtime_event_receive_cut: None,
                bus,
                event_log: EventLog::new(),
                replayable_harness_notices: Vec::new(),
                last_live_egress_lag_warning: None,
                debug_log: None,
                debug_log_poisoned: false,
                publication: PublicationState::default(),
            },
            session_runtime: SessionRuntimeState {
                persistence_owner,
                state_dir: state_dir.clone(),
                store,
                agent_store,
                storage_mode,
                runtime_harness_path: None,
                project_root,
                current_session_id: eager_session_id
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                session_pinned: false,
                current_session_generation: SessionGeneration::default(),
                current_session_start_reason: launch.mode.reason(),
                shutdown_published: false,
                lifecycle_messages: Vec::new(),
                user_interaction_order: HashMap::new(),
                next_user_interaction_order: 1,
                precommitted_user_interactions: HashMap::new(),
                turn_state: TurnState::Idle,
                current_session_state: CurrentSessionState::default(),
            },
            config: HarnessConfigState {
                provider_settings_snapshots: BTreeMap::new(),
                accepted_harness_settings: harness_settings.clone(),
                available_roles,
                disabled_role_reasons: HashMap::new(),
                available_role_groups,
                inter_session_receivers,
                custom_prompts,
                role_overrides,
                tool_policy: harness_settings.tool_policy.clone(),
                selected_role,
                selected_model,
                agent_id_template: harness_settings.agent_id_template.clone(),
                agent_display_name_template: harness_settings.agent_display_name_template.clone(),
            },
            input_wait_timeout_bounds: harness_settings.wait_timeout_bounds(),
            provider_cache_refresh: harness_settings.provider_cache_refresh,
            system_prompt_templates,
        });

        if storage_mode.is_durable() {
            // Debug log lives next to the eager-init session's events file
            // so the session dir stays self-contained: `events.cbor` +
            // `events.jsonl` + `meta.json` + `lock`.
            let _ = harness.enable_debug_log(&sessions_dir.join(eager_session_id))?;
        }

        harness.install_internal_tool_handlers(internal_tool_handlers);
        if launch.mode.is_resume() {
            harness.rehydrate_agents_from_session();
        } else {
            harness.restore_existing_session_accounting_without_runtime_rehydration();
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
            launch.mode.reason(),
        );
        harness.wait_for_session_init()?;
        harness.activate_replayed_prompt_occurrences();
        harness.ensure_selected_role_available_after_required_skill_validation()?;
        if launch.mode.is_resume() {
            harness.finalize_restored_standalone_costs();
        }
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
                mode: HarnessSessionLaunchMode::from_reason(eager_session_start_reason),
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
                mode: HarnessSessionLaunchMode::from_reason(eager_session_start_reason),
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

        if launch.mode.is_resume() {
            harness.rehydrate_agents_from_session();
        } else {
            harness.restore_existing_session_accounting_without_runtime_rehydration();
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
            launch.mode.reason(),
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
        if launch.mode.is_resume() {
            harness.finalize_restored_standalone_costs();
        }
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
        let (store, agent_store, persistence_owner) = Self::open_startup_stores(
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
            persistence_owner,
            eager_session_id: eager_session_id.to_owned(),
            launch,
            harness_settings,
            roles,
            project_root,
        })?;
        harness.config.provider_settings_snapshots = provider_settings_snapshots;
        harness.extensions.enabled_names = config
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
    ) -> Result<
        (
            SessionStore,
            AgentStore,
            Option<Arc<tau_core::SemanticPersistenceOwner>>,
        ),
        HarnessError,
    > {
        let durable_session = !storage_mode.is_ephemeral();
        let durable_agents = !storage_mode.is_memory_only() && !memory_only_agent_store;
        let persistence_owner = (durable_session || durable_agents)
            .then(|| {
                tau_core::SemanticPersistenceOwner::new(Default::default())
                    .map(Arc::new)
                    .map_err(|error| HarnessError::Participant(error.to_string()))
            })
            .transpose()?;
        let store = if storage_mode.is_ephemeral() {
            SessionStore::open_ephemeral(sessions_dir)?
        } else {
            SessionStore::open_managed(
                sessions_dir,
                persistence_owner
                    .as_ref()
                    .expect("durable session has owner")
                    .clone(),
            )?
        };
        let agent_store = if storage_mode.is_memory_only() || memory_only_agent_store {
            AgentStore::open_memory_only(state_dir.join("agents"))
        } else {
            AgentStore::open_managed(
                state_dir.join("agents"),
                persistence_owner
                    .as_ref()
                    .expect("durable agent has owner")
                    .clone(),
            )?
        };
        Ok((store, agent_store, persistence_owner))
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
        if parts.launch.storage_mode.is_durable() {
            parts
                .store
                .prepare_session(&parts.eager_session_id, parts.launch.mode.preparation())?;
        } else if parts.launch.mode.is_resume() {
            let _ = parts
                .store
                .lock_and_load_existing_session(&parts.eager_session_id)?;
        } else {
            let _ = parts.store.lock_and_load_session(&parts.eager_session_id)?;
        }
        if parts.launch.mode.is_resume() {
            let sessions_dir = parts.store.sessions_dir().to_path_buf();
            Self::create_resumed_harness_log_after_lock(
                &sessions_dir,
                &parts.eager_session_id,
                parts.launch.storage_mode,
            )?;
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
            runtime_io: RuntimeIoState {
                tx,
                rx,
                component_ingress_tx,
                component_ingress,
                pending_runtime_event: None,
                #[cfg(test)]
                runtime_event_receive_cut: None,
                bus,
                event_log: EventLog::new(),
                replayable_harness_notices: Vec::new(),
                last_live_egress_lag_warning: None,
                debug_log: None,
                debug_log_poisoned: false,
                publication: PublicationState::default(),
            },
            session_runtime: SessionRuntimeState {
                persistence_owner: parts.persistence_owner,
                state_dir: parts.state_dir,
                store: parts.store,
                agent_store: parts.agent_store,
                storage_mode: parts.launch.storage_mode,
                runtime_harness_path: None,
                project_root: parts.project_root,
                current_session_id: parts
                    .eager_session_id
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                session_pinned: false,
                current_session_generation: SessionGeneration::default(),
                current_session_start_reason: parts.launch.mode.reason(),
                shutdown_published: false,
                lifecycle_messages: Vec::new(),
                user_interaction_order: HashMap::new(),
                next_user_interaction_order: 1,
                precommitted_user_interactions: HashMap::new(),
                turn_state: TurnState::Idle,
                current_session_state: CurrentSessionState::default(),
            },
            config: HarnessConfigState {
                provider_settings_snapshots: BTreeMap::new(),
                accepted_harness_settings: parts.harness_settings.clone(),
                available_roles: parts.roles.available_roles,
                disabled_role_reasons: HashMap::new(),
                available_role_groups: parts.roles.available_role_groups,
                inter_session_receivers: parts.roles.inter_session_receivers,
                custom_prompts,
                role_overrides: parts.roles.role_overrides,
                tool_policy: parts.harness_settings.tool_policy.clone(),
                selected_role: parts.roles.selected_role,
                selected_model: parts.roles.selected_model,
                agent_id_template: parts.harness_settings.agent_id_template.clone(),
                agent_display_name_template: parts
                    .harness_settings
                    .agent_display_name_template
                    .clone(),
            },
            input_wait_timeout_bounds: parts.harness_settings.wait_timeout_bounds(),
            provider_cache_refresh: parts.harness_settings.provider_cache_refresh,
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
        if self.session_runtime.storage_mode.is_ephemeral() {
            return Ok(());
        }
        // Commit canonical existence before creating session-owned diagnostics.
        // The creating lock and directory remain incomplete scaffolding if this
        // replacement fails.
        self.session_runtime
            .store
            .record_session_meta(eager_session_id)?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session metadata recorded");
        let _ = self.enable_debug_log(&sessions_dir.join(eager_session_id))?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "debug event log enabled");
        Ok(())
    }
}
