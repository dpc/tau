use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread as path_std_thread;
use std::thread::JoinHandle;

use iroh::endpoint::presets;
use iroh::{Endpoint, RelayConfig};
use rand::RngCore;
use rand::rngs::OsRng;
use tau_client::{
    ClientError, ClientHandle, ExtensionBuilder, RawConfigureContext, RawEventContext,
    TauExtension, TauExtensionRunner,
};
use tau_proto::{Event, EventSelector};
use tau_swarm_api::{
    Agent, AgentActivity, AgentId, AgentNavigationMode, AgentWorkStatus, ApplicationIncarnationId,
    Hostname, SessionId, SessionIdentity, TaskName,
};
use tau_swarm_client::{Client, ExpectedPeer};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::{runtime as path_tokio_runtime, sync as path_tokio_sync};

use crate::application::{BlockerSubmission, CommandState, PromptSubmission, SwarmApplication};
use crate::config::{ExtConfig, ResolvedConfig};
use crate::projection::SessionProjection;
use crate::tools::BlockerRecord;
use crate::worker_health::WorkerHealth;

/// Tracing target for the bundled extension.
pub const LOG_TARGET: &str = "std-swarm";

/// Replay/live accumulator for one Tau agent load epoch.
#[derive(Clone)]
struct AgentDraft {
    /// Latest explicit Tau display name, if one has been recorded.
    name: Option<String>,
    /// Latest canonical semantic work status.
    work_status: AgentWorkStatus,
    /// Latest independent running/waiting state.
    activity: AgentActivity,
    /// Latest faithful navigation classification.
    navigation_mode: AgentNavigationMode,
    /// Latest complete watch replacement set.
    watches: BTreeSet<AgentId>,
    /// Whether current session membership includes this agent.
    loaded: bool,
    /// Whether this load epoch reached a successful agent replay boundary.
    replay_valid: bool,
}

impl AgentDraft {
    fn new() -> Self {
        Self {
            name: None,
            work_status: AgentWorkStatus::Unreported,
            activity: AgentActivity::Waiting,
            navigation_mode: AgentNavigationMode::Active,
            watches: BTreeSet::new(),
            loaded: false,
            replay_valid: false,
        }
    }

    fn publication(&self, id: AgentId) -> Agent {
        Agent {
            id,
            name: self.name.clone().unwrap_or_default(),
            work_status: self.work_status.clone(),
            activity: self.activity,
            navigation_mode: self.navigation_mode,
            watches: self.watches.clone(),
        }
    }
}

type Completion = oneshot::Sender<Result<(), String>>;

/// Exact canonical-loopback identity for one submitted internal prompt.
#[derive(Clone, Eq, Hash, PartialEq)]
struct PendingKey {
    /// Target agent in the current Tau session.
    agent_id: AgentId,
    /// Command/correlation identifier installed as Tau's context ID.
    ctx_id: String,
    /// Exact submitted text required for authoritative loopback.
    text: String,
}

/// Shared exact loopbacks awaiting canonical Tau completion.
type PendingCompletions = Arc<Mutex<HashMap<PendingKey, Completion>>>;

/// Owned Swarm worker and cooperative shutdown channel.
struct Worker {
    /// Cooperative cancellation observed by the Tokio owner.
    shutdown: watch::Sender<bool>,
    /// Join handle that prevents detached worker lifetime.
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn stop(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Mutable Tau-side owner for one process and current session incarnation.
pub(crate) struct SwarmRuntime {
    /// Collision-resistant identity retained for this extension process.
    pub(crate) application_incarnation_id: ApplicationIncarnationId,
    /// Outbound Tau handle installed by accepted Configure.
    handle: Option<ClientHandle>,
    /// Immutable resolved configuration for this process.
    pub(crate) config: Option<ResolvedConfig>,
    /// Current Tau logical session, cleared on shutdown.
    session_id: Option<tau_proto::SessionId>,
    /// Whether the current catch-up crossed its coherent boundary.
    pub(crate) replay_complete: bool,
    /// False after a publication bound invalidates this incarnation.
    pub(crate) projection_valid: bool,
    /// Replay/live drafts keyed independently from membership.
    agents: HashMap<AgentId, AgentDraft>,
    /// Coherent publication state and revision.
    pub(crate) projection: Arc<tokio::sync::Mutex<SessionProjection>>,
    /// Wakes Swarm change readers after committed projection mutation.
    pub(crate) changed: Arc<tokio::sync::Notify>,
    /// Exact target/context/text loopbacks awaiting canonical Tau facts.
    pending: PendingCompletions,
    /// No-eviction command results retained for this process incarnation.
    commands: Option<Arc<tokio::sync::Mutex<CommandState>>>,
    /// Owned worker for the current replay-complete session.
    worker: Option<Worker>,
    /// Authoritative health of the current worker generation.
    pub(crate) worker_health: WorkerHealth,
    /// Full current-session blocker lifecycle history in opening order.
    pub(crate) blocker_history: Arc<Mutex<Vec<BlockerRecord>>>,
}

impl SwarmRuntime {
    /// Creates empty process state before Configure and session replay.
    pub(crate) fn new() -> Self {
        let mut incarnation = [0_u8; 32];
        OsRng.fill_bytes(&mut incarnation);
        Self {
            application_incarnation_id: ApplicationIncarnationId::from_bytes(incarnation),
            handle: None,
            config: None,
            session_id: None,
            replay_complete: false,
            projection_valid: true,
            agents: HashMap::new(),
            projection: Arc::new(path_tokio_sync::Mutex::new(SessionProjection::new(4_096))),
            changed: Arc::new(path_tokio_sync::Notify::new()),
            pending: Arc::new(Mutex::new(HashMap::new())),
            commands: None,
            worker: None,
            worker_health: WorkerHealth::indeterminate(),
            blocker_history: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn reset_session(&mut self, session_id: Option<tau_proto::SessionId>) {
        self.stop_worker();
        self.session_id = session_id;
        self.replay_complete = false;
        self.projection_valid = true;
        self.agents.clear();
        self.blocker_history
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        let capacity = self
            .config
            .as_ref()
            .map_or(4_096, |config| config.limits.change_history_entries);
        let projection = if let Some(config) = &self.config {
            SessionProjection::new(capacity)
                .with_byte_limits(
                    config.limits.change_history_bytes,
                    config.limits.publication_bytes,
                )
                .with_task_info_limit(config.limits.task_info_entries)
        } else {
            SessionProjection::new(capacity)
        };
        self.projection = Arc::new(path_tokio_sync::Mutex::new(projection));
        self.changed = Arc::new(path_tokio_sync::Notify::new());
    }

    fn stop_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop();
        }
        self.worker_health = WorkerHealth::indeterminate();
    }

    fn publish_agent(&mut self, id: &AgentId) -> Result<(), ClientError> {
        if !self.projection_valid {
            return Ok(());
        }
        let Some(draft) = self
            .agents
            .get(id)
            .filter(|draft| draft.loaded && draft.replay_valid)
            .cloned()
        else {
            return Ok(());
        };
        let loaded = self.agents.values().filter(|draft| draft.loaded).count();
        let watches = self
            .agents
            .values()
            .filter(|draft| draft.loaded)
            .try_fold(0_usize, |total, draft| {
                total.checked_add(draft.watches.len())
            });
        let limits = &self
            .config
            .as_ref()
            .ok_or_else(|| ClientError::handler("Swarm is not configured"))?
            .limits;
        if limits.agent_entries < loaded
            || watches.is_none_or(|watches| limits.watch_entries < watches)
        {
            self.invalidate_projection();
            return Ok(());
        }
        if self
            .projection
            .blocking_lock()
            .upsert_agent(draft.publication(id.clone()))
            .is_err()
        {
            self.invalidate_projection();
            return Ok(());
        }
        self.changed.notify_waiters();
        Ok(())
    }

    fn invalidate_projection(&mut self) {
        if !self.projection_valid {
            return;
        }
        self.projection_valid = false;
        self.stop_worker();
        self.agents.clear();
        self.blocker_history
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        let capacity = self
            .config
            .as_ref()
            .map_or(4_096, |config| config.limits.change_history_entries);
        let projection = if let Some(config) = &self.config {
            SessionProjection::new(capacity)
                .with_byte_limits(
                    config.limits.change_history_bytes,
                    config.limits.publication_bytes,
                )
                .with_task_info_limit(config.limits.task_info_entries)
        } else {
            SessionProjection::new(capacity)
        };
        self.projection = Arc::new(path_tokio_sync::Mutex::new(projection));
        self.changed = Arc::new(path_tokio_sync::Notify::new());
        if let Some(handle) = &self.handle {
            let _ = handle.request_notice_detached(
                "Tau Swarm projection exceeded configured agent/watch bounds; publication is disabled until a fresh session replay",
                tau_proto::NoticeLevel::Warning,
            );
        }
    }

    fn complete_prompt(&self, agent_id: &str, ctx_id: Option<&str>, text: &str) {
        let Some(ctx_id) = ctx_id else { return };
        let key = PendingKey {
            agent_id: AgentId::new(agent_id),
            ctx_id: ctx_id.to_owned(),
            text: text.to_owned(),
        };
        if let Some(completion) = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&key)
        {
            let _ = completion.send(Ok(()));
        }
    }

    fn start_worker(&mut self) -> Result<(), String> {
        if self.worker.is_some() || !self.replay_complete {
            return Ok(());
        }
        let config = self
            .config
            .clone()
            .ok_or_else(|| "Swarm configuration is unavailable".to_owned())?;
        let session_id = self
            .session_id
            .clone()
            .ok_or_else(|| "Tau session identity is unavailable".to_owned())?;
        let handle = self
            .handle
            .clone()
            .ok_or_else(|| "Tau output handle is unavailable".to_owned())?;
        let projection = Arc::clone(&self.projection);
        let changed = Arc::clone(&self.changed);
        let pending = Arc::clone(&self.pending);
        let commands = Arc::clone(
            self.commands
                .as_ref()
                .ok_or_else(|| "Swarm command state is unavailable".to_owned())?,
        );
        let application_incarnation_id = self.application_incarnation_id.clone();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (prompts_tx, prompts_rx) = mpsc::channel(config.limits.submission_queue_entries);
        let (blockers_tx, blockers_rx) = mpsc::channel(config.limits.submission_queue_entries);
        let identity = SessionIdentity::new(
            Hostname::new(config.hostname.clone()),
            SessionId::new(session_id.as_str()),
        );
        let application = Arc::new(
            SwarmApplication::new(identity, projection, changed, prompts_tx, blockers_tx)
                .with_command_state(commands, config.command_timeout)
                .with_blocker_history(
                    Arc::clone(&self.blocker_history),
                    config.limits.blocker_bytes,
                ),
        );
        let worker_health = WorkerHealth::running();
        let task_health = worker_health.clone();
        let thread = path_std_thread::Builder::new()
            .name("tau-ext-swarm".into())
            .spawn(move || {
                let terminal = task_health.terminal_guard();
                let result = worker_main(
                    config,
                    (application, application_incarnation_id),
                    handle.clone(),
                    pending,
                    prompts_rx,
                    blockers_rx,
                    shutdown_rx,
                );
                finish_worker(result, terminal, |error| {
                    let _ = handle.request_notice_detached(
                        format!("Tau Swarm worker stopped: {}", bounded_error(error)),
                        tau_proto::NoticeLevel::Warning,
                    );
                });
            })
            .map_err(|error| format!("failed to start Swarm worker: {error}"))?;
        self.worker_health = worker_health;
        self.worker = Some(Worker {
            shutdown: shutdown_tx,
            thread: Some(thread),
        });
        Ok(())
    }
}

/// Retires publication authority before optional reporting of a worker error.
fn finish_worker(
    result: Result<(), String>,
    terminal: crate::worker_health::WorkerTerminalGuard,
    report_error: impl FnOnce(&str),
) {
    drop(terminal);
    if let Err(error) = result {
        report_error(&error);
    }
}

impl Drop for SwarmRuntime {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

async fn bridge_prompt(
    submission: PromptSubmission,
    handle: &ClientHandle,
    pending: &Arc<Mutex<HashMap<PendingKey, Completion>>>,
) {
    submit_loopback(
        submission.agent_id,
        submission.text,
        submission.ctx_id,
        submission.completion,
        handle,
        pending,
    );
}

async fn bridge_blocker(
    submission: BlockerSubmission,
    handle: &ClientHandle,
    pending: &Arc<Mutex<HashMap<PendingKey, Completion>>>,
) {
    submit_loopback(
        submission.agent_id,
        submission.text,
        submission.ctx_id,
        submission.completion,
        handle,
        pending,
    );
}

fn submit_loopback(
    agent_id: AgentId,
    text: String,
    ctx_id: String,
    completion: Completion,
    handle: &ClientHandle,
    pending: &Arc<Mutex<HashMap<PendingKey, Completion>>>,
) {
    let key = PendingKey {
        agent_id: agent_id.clone(),
        ctx_id: ctx_id.clone(),
        text: text.clone(),
    };
    pending
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(key.clone(), completion);
    let Ok(tau_agent_id) = tau_proto::AgentId::parse(agent_id.as_str()) else {
        pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&key);
        return;
    };
    let event = Event::ExtInternalPromptSubmitRequest(tau_proto::ExtInternalPromptSubmitRequest {
        agent_id: tau_agent_id,
        text,
        ctx_id: Some(ctx_id),
        activation_kind: None,
    });
    if handle.emit_transient_detached(event).is_err() {
        pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&key);
    }
}

fn worker_main(
    config: ResolvedConfig,
    application: (Arc<SwarmApplication>, ApplicationIncarnationId),
    handle: ClientHandle,
    pending: Arc<Mutex<HashMap<PendingKey, Completion>>>,
    mut prompts: mpsc::Receiver<PromptSubmission>,
    mut blockers: mpsc::Receiver<BlockerSubmission>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    let runtime = path_tokio_runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async move {
        let endpoint = Endpoint::builder(presets::N0)
            .bind()
            .await
            .map_err(|error| error.to_string())?;
        if let Some(relay) = config.relay.as_ref() {
            endpoint
                .insert_relay(relay.clone(), Arc::new(RelayConfig::from(relay.clone())))
                .await;
        }
        let expected = ExpectedPeer::new(config.endpoint.id.as_bytes());
        let connector = tau_swarm_iroh::IrohConnector::new(endpoint.clone(), config.endpoint);
        let mut seed = OsRng.next_u64();
        if seed == 0 {
            seed = 1;
        }
        let backoff = config.reconnect.backoff(seed);
        let client = Client::new(
            application.0,
            application.1,
            connector,
            expected,
            config.credential,
            backoff,
        );
        tokio::select! {
            result = client.run() => result.map_err(|error| {
                format!("{:?}: {}", error.kind(), bounded_error(&error.to_string()))
            }),
            _ = async {
                loop {
                    tokio::select! {
                        Some(submission) = prompts.recv() => {
                            bridge_prompt(submission, &handle, &pending).await;
                        }
                        Some(submission) = blockers.recv() => {
                            bridge_blocker(submission, &handle, &pending).await;
                        }
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                break;
                            }
                        }
                    }
                }
            } => Ok(()),
        }?;
        endpoint.close().await;
        Ok(())
    })
}

fn bounded_error(error: &str) -> &str {
    const MAXIMUM_BYTES: usize = 4 * 1024;
    if error.len() <= MAXIMUM_BYTES {
        return error;
    }
    let mut end = MAXIMUM_BYTES;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    &error[..end]
}

/// Registers the std-swarm protocol, projection handlers, and tools.
struct SwarmExtension;

impl TauExtension for SwarmExtension {
    type State = SwarmRuntime;

    fn name(&self) -> &'static str {
        "tau-ext-swarm"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .ready_message("swarm ready")
            .configure_raw(handle_configure);
        for event in [
            tau_proto::EventName::SESSION_STARTED,
            tau_proto::EventName::SESSION_SHUTDOWN,
            tau_proto::EventName::SESSION_AGENT_LOADED,
            tau_proto::EventName::SESSION_AGENT_UNLOADED,
            tau_proto::EventName::SESSION_REPLAY_COMPLETE,
            tau_proto::EventName::AGENT_STARTED,
            tau_proto::EventName::AGENT_DISPLAY_NAME_SET,
            tau_proto::EventName::AGENT_STATS_UPDATED,
            tau_proto::EventName::AGENT_WATCHES_UPDATED,
            tau_proto::EventName::AGENT_REPLAY_COMPLETE,
        ] {
            let selector = EventSelector::Exact(event);
            builder
                .on_raw_restore(selector.clone(), handle_event)
                .on_raw_live(selector, handle_event);
        }
        for event in [
            tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            tau_proto::EventName::AGENT_PROMPT_STEERED,
        ] {
            builder.on_raw_live(EventSelector::Exact(event), handle_canonical_prompt);
        }
        crate::tools::register(builder);
    }
}

fn handle_configure(cx: RawConfigureContext<'_, SwarmRuntime>) -> Result<(), ClientError> {
    if cx.state.config.is_some() {
        return Err(ClientError::handler(
            "Swarm configuration is immutable until extension restart",
        ));
    }
    let config = cx
        .parse_config::<ExtConfig>()?
        .resolve(cx.secrets())
        .map_err(ClientError::handler)?;
    cx.state.handle = Some(cx.handle());
    cx.state.projection = Arc::new(path_tokio_sync::Mutex::new(
        SessionProjection::new(config.limits.change_history_entries)
            .with_byte_limits(
                config.limits.change_history_bytes,
                config.limits.publication_bytes,
            )
            .with_task_info_limit(config.limits.task_info_entries),
    ));
    cx.state.config = Some(config);
    let limits = &cx.state.config.as_ref().expect("installed config").limits;
    cx.state.commands = Some(Arc::new(path_tokio_sync::Mutex::new(CommandState::new(
        limits.command_entries,
        limits.command_bytes,
    ))));
    tracing::info!(target: LOG_TARGET, "swarm configured");
    Ok(())
}

fn handle_event(cx: RawEventContext<'_, SwarmRuntime>) -> Result<(), ClientError> {
    let event = cx.event().clone();
    fold_event(cx.state, &event)
}

fn fold_event(state: &mut SwarmRuntime, event: &Event) -> Result<(), ClientError> {
    if !state.projection_valid
        && !matches!(event, Event::SessionStarted(_) | Event::SessionShutdown(_))
    {
        return Ok(());
    }
    match event {
        Event::SessionStarted(event) => {
            state.reset_session(Some(event.session_id.clone()));
        }
        Event::SessionShutdown(event) if state.session_id.as_ref() == Some(&event.session_id) => {
            state.reset_session(None);
        }
        Event::SessionAgentLoaded(event)
            if state.session_id.as_ref() == Some(&event.session_id) =>
        {
            let id = AgentId::new(event.agent_id.as_str());
            state
                .agents
                .entry(id.clone())
                .or_insert_with(AgentDraft::new)
                .loaded = true;
            // Publication waits for this load epoch's replay validity boundary.
        }
        Event::SessionAgentUnloaded(event)
            if state.session_id.as_ref() == Some(&event.session_id) =>
        {
            let id = AgentId::new(event.agent_id.as_str());
            state.agents.remove(&id);
            let _ = state.projection.blocking_lock().remove_agent(&id);
            state.changed.notify_waiters();
            state
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .retain(|key, _| key.agent_id != id);
        }
        Event::AgentStarted(event) => {
            let id = AgentId::new(event.agent_id.as_str());
            let draft = state
                .agents
                .entry(id.clone())
                .or_insert_with(AgentDraft::new);
            if let Some(name) = &event.display_name {
                draft.name = Some(name.clone());
            }
            state.publish_agent(&id)?;
        }
        Event::AgentReplayComplete(event)
            if event.error.is_none()
                && event
                    .session_id
                    .as_ref()
                    .is_none_or(|session| state.session_id.as_ref() == Some(session)) =>
        {
            let id = AgentId::new(event.agent_id.as_str());
            state
                .agents
                .entry(id.clone())
                .or_insert_with(AgentDraft::new)
                .replay_valid = true;
            state.publish_agent(&id)?;
        }
        Event::AgentReplayComplete(event)
            if event.error.is_some()
                && event
                    .session_id
                    .as_ref()
                    .is_none_or(|session| state.session_id.as_ref() == Some(session)) =>
        {
            state.invalidate_projection();
        }
        Event::AgentDisplayNameSet(event) => {
            let id = AgentId::new(event.agent_id.as_str());
            state
                .agents
                .entry(id.clone())
                .or_insert_with(AgentDraft::new)
                .name
                .replace(event.display_name.clone());
            state.publish_agent(&id)?;
        }
        Event::AgentStatsUpdated(event) if state.session_id.as_ref() == Some(&event.session_id) => {
            let id = AgentId::new(event.agent_id.as_str());
            let draft = state
                .agents
                .entry(id.clone())
                .or_insert_with(AgentDraft::new);
            draft.work_status =
                swarm_work_status(&event.work_status).map_err(ClientError::handler)?;
            draft.activity = match event.runtime_state {
                tau_proto::AgentRuntimeState::Running => AgentActivity::Running,
                tau_proto::AgentRuntimeState::Idle => AgentActivity::Waiting,
            };
            draft.navigation_mode = match event.navigation_mode {
                tau_proto::AgentNavigationMode::Active => AgentNavigationMode::Active,
                tau_proto::AgentNavigationMode::ActiveAuto => AgentNavigationMode::ActiveAuto,
                tau_proto::AgentNavigationMode::Suspended => AgentNavigationMode::Suspended,
            };
            state.publish_agent(&id)?;
        }
        Event::AgentWatchesUpdated(event)
            if state.session_id.as_ref() == Some(&event.session_id) =>
        {
            let id = AgentId::new(event.watcher_id.as_str());
            state
                .agents
                .entry(id.clone())
                .or_insert_with(AgentDraft::new)
                .watches = event
                .watched_agent_ids
                .iter()
                .map(|id| AgentId::new(id.as_str()))
                .collect();
            state.publish_agent(&id)?;
        }
        Event::SessionReplayComplete(event)
            if state.session_id.as_ref() == Some(&event.session_id)
                && event.error.is_none()
                && state.projection_valid =>
        {
            state.replay_complete = true;
            state.start_worker().map_err(ClientError::handler)?;
        }
        Event::SessionReplayComplete(event)
            if state.session_id.as_ref() == Some(&event.session_id) && event.error.is_some() =>
        {
            state.invalidate_projection();
        }
        _ => {}
    }
    Ok(())
}

/// Converts Tau's canonical work-status snapshot into the validated Swarm v0
/// domain representation.
fn swarm_work_status(
    status: &tau_proto::SessionAgentWorkStatus,
) -> Result<AgentWorkStatus, String> {
    let task_name = |title: Option<&str>| {
        title
            .ok_or_else(|| "reported work status is missing its task name".to_owned())
            .and_then(canonical_task_name)
    };
    match status.phase() {
        tau_proto::AgentWorkStatusPhase::Unreported => Ok(AgentWorkStatus::Unreported),
        tau_proto::AgentWorkStatusPhase::Working => Ok(AgentWorkStatus::Working {
            task_name: task_name(status.title())?,
        }),
        tau_proto::AgentWorkStatusPhase::Done => Ok(AgentWorkStatus::Done {
            task_name: task_name(status.title())?,
        }),
        tau_proto::AgentWorkStatusPhase::Blocked => Ok(AgentWorkStatus::Blocked {
            task_name: task_name(status.title())?,
        }),
        // Swarm v0 has no matching work-status phase. Its orthogonal activity
        // remains independently derived from Tau's runtime state.
        tau_proto::AgentWorkStatusPhase::Waiting => Ok(AgentWorkStatus::Working {
            task_name: task_name(status.title())?,
        }),
        tau_proto::AgentWorkStatusPhase::Unknown => Ok(AgentWorkStatus::Unknown {
            last_task_name: status.title().map(canonical_task_name).transpose()?,
        }),
    }
}

/// Validates a task name without accepting a noncanonical source spelling.
fn canonical_task_name(title: &str) -> Result<TaskName, String> {
    let task_name = TaskName::new(title).map_err(|error| error.to_string())?;
    if task_name.as_str() != title {
        return Err("reported work status task name is not canonical".to_owned());
    }
    Ok(task_name)
}

fn handle_canonical_prompt(cx: RawEventContext<'_, SwarmRuntime>) -> Result<(), ClientError> {
    let event = cx.event().clone();
    fold_canonical_prompt(cx.state, &event);
    Ok(())
}

fn fold_canonical_prompt(state: &SwarmRuntime, event: &Event) {
    match event {
        Event::AgentPromptSubmitted(event) => state.complete_prompt(
            event.agent_id.as_str(),
            event.ctx_id.as_deref(),
            &event.text,
        ),
        Event::AgentPromptSteered(event) => state.complete_prompt(
            event.agent_id.as_str(),
            event.ctx_id.as_deref(),
            &event.text,
        ),
        _ => {}
    }
}

/// Runs the extension over stdio.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_client::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the extension over arbitrary protocol streams.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send,
{
    TauExtensionRunner::new(SwarmExtension)
        .run(reader, writer, SwarmRuntime::new())
        .map(|_| ())
        .map_err(Into::into)
}

#[cfg(test)]
mod tests;
