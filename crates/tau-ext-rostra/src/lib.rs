//! Relay-only Rostra tools backed by an extension-owned database.
//!
//! `ARCH-tau-ext-rostra` records the persistent-state, synchronization, and
//! hostile-content boundaries implemented here.

mod cursor;
mod notification_page;
mod notification_pending;
mod notification_registration;
mod notification_state;
mod notification_tool;
mod notifications;
mod post_rate_limit;
mod projection;
mod specs;
mod tools;

use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::str::FromStr as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rostra_client::{Client, Database};
use rostra_core::id::RostraIdSecretKey;
use tau_client::{ClientError, ClientResult, ExtensionBuilder, TauExtension};
use tau_proto::{Event, EventSelector, SessionAgentLoaded, SessionAgentUnloaded, ToolCancelled};
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore, oneshot};
use tokio::task::AbortHandle;

use crate::post_rate_limit::{PostRateLimit, PostRateLimitWindow};

/// Logging target used by this extension.
pub const LOG_TARGET: &str = "rostra";
/// Maximum records returned by one timeline call.
pub(crate) const MAX_PAGE_SIZE: usize = 50;
/// Default records returned by one timeline call.
pub(crate) const DEFAULT_PAGE_SIZE: usize = 20;
/// Maximum Unicode scalar values in a list excerpt.
pub(crate) const MAX_EXCERPT_CHARS: usize = 240;
/// Maximum UTF-8 bytes returned for detailed Djot source.
pub(crate) const MAX_DJOT_BYTES: usize = 64 * 1024;

#[cfg(not(test))]
const TOOL_DEADLINE: Duration = Duration::from_secs(10);
/// This gives the cancellation half of the protocol test scheduling margin
/// while still exercising a retained publication without production's
/// ten-second wait.
#[cfg(test)]
const TOOL_DEADLINE: Duration = Duration::from_secs(1);
const MAX_CONCURRENT_TOOLS: usize = 8;

/// Run the extension over stdio.
///
/// # Errors
///
/// Returns an error when protocol I/O or runtime construction fails.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_client::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Run the extension over the supplied protocol streams.
///
/// # Errors
///
/// Returns an error when protocol I/O or runtime construction fails.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    let runtime = RuntimeBuilder::new_multi_thread().enable_all().build()?;
    let state = RostraState {
        client: None,
        identity_secret: None,
        runtime: Some(runtime),
        running: Arc::new(Mutex::new(HashMap::new())),
        permits: Arc::new(Semaphore::new(MAX_CONCURRENT_TOOLS)),
        write_lock: Arc::new(AsyncMutex::new(())),
        post_rate_limit: PostRateLimit::default(),
        post_rate_limit_window: Arc::new(Mutex::new(PostRateLimitWindow::default())),
        notifications: Arc::new(Mutex::new(notification_state::State::default())),
        notifications_wake: Arc::new(Notify::new()),
        notifications_task: None,
    };
    tau_client::TauExtensionRunner::new(RostraExtension)
        .run_detached_writer(reader, writer, state)?;
    Ok(())
}

/// Strict public configuration for one Rostra identity.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtConfig {
    /// Name of the Tau-managed mnemonic secret for this identity.
    identity_mnemonic_secret: String,
    /// Rolling quota for self-authored Rostra social-post events.
    #[serde(default)]
    post_rate_limit: PostRateLimit,
}

/// Runtime state owned by one extension process.
pub(crate) struct RostraState {
    /// Full client; declared before the runtime so shutdown drops it first.
    pub(crate) client: Option<Arc<Client>>,
    /// Signing key retained until the client and its active tasks are dropped.
    identity_secret: Option<RostraIdSecretKey>,
    /// Executor that owns Rostra and bounded tool tasks.
    pub(crate) runtime: Option<tokio::runtime::Runtime>,
    /// Abort handles for wrappers that can still produce a terminal result.
    running: Arc<Mutex<HashMap<tau_proto::ToolCallId, RunningCall>>>,
    /// Process-wide bound on concurrent tool work.
    permits: Arc<Semaphore>,
    /// Serializes lazy activation and all authenticated publications.
    write_lock: Arc<AsyncMutex<()>>,
    /// Configured rolling quota for post, reply, and reaction publications.
    post_rate_limit: PostRateLimit,
    /// Runtime-only current-window admissions for the configured identity.
    post_rate_limit_window: Arc<Mutex<PostRateLimitWindow>>,
    /// Extension-owned agent notification preferences and feed checkpoints.
    pub(crate) notifications: Arc<Mutex<notification_state::State>>,
    /// Wakes reconciliation after local policy or lifecycle mutations.
    pub(crate) notifications_wake: Arc<Notify>,
    /// Lossy-hint reconciliation worker for the current Rostra client.
    notifications_task: Option<AbortHandle>,
}

impl Drop for RostraState {
    fn drop(&mut self) {
        abort_all(&self.running);
        if let Some(task) = self.notifications_task.take() {
            task.abort();
        }
        self.client = None;
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(1));
        }
    }
}

/// Cancellation metadata retained until a worker completes.
struct RunningCall {
    /// Tokio cancellation handle.
    abort: AbortHandle,
    /// Tool name needed by the cancellation terminal event.
    tool_name: tau_proto::ToolName,
    /// Output handle used without blocking protocol ingestion.
    handle: tau_client::ClientHandle,
}

/// Tau extension declaration.
struct RostraExtension;

impl TauExtension for RostraExtension {
    type State = RostraState;

    fn name(&self) -> &'static str {
        "tau-ext-rostra"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .configure_with_error::<ExtConfig>(
                |cx| {
                    configure(cx.state, cx.configure, cx.config)?;
                    start_notifications(cx.state, cx.handle);
                    Ok(())
                },
                |cx| {
                    cx.state.client = None;
                    cx.state.identity_secret = None;
                    cx.state.post_rate_limit = PostRateLimit::default();
                    *cx.state
                        .post_rate_limit_window
                        .lock()
                        .expect("post rate-limit state lock") = PostRateLimitWindow::default();
                    abort_all(&cx.state.running);
                    if let Some(task) = cx.state.notifications_task.take() {
                        task.abort();
                    }
                    cx.state
                        .notifications
                        .lock()
                        .expect("notification state lock")
                        .clear();
                },
            )
            .message_bridge()
            .scoped_tool(
                tau_proto::ToolName::new(specs::NOTIFICATIONS_TOOL),
                |_scope| {
                    Ok(tau_proto::ToolRegistrationDeclared {
                        tool: specs::notifications_spec(),
                        tool_group: Some(specs::tool_group()),
                        prompt_fragment: None,
                    })
                },
                notification_tool::handle,
            )
            .on_restore::<SessionAgentLoaded>(apply_agent_loaded)
            .on::<SessionAgentLoaded>(apply_agent_loaded)
            .on_restore::<SessionAgentUnloaded>(apply_agent_unloaded)
            .on::<SessionAgentUnloaded>(apply_agent_unloaded)
            .on::<tau_proto::AgentReplayComplete>(apply_agent_replay_complete)
            .on_raw_live(
                EventSelector::Exact(tau_proto::EventName::MESSAGE_DELIVERED),
                apply_notification_checkpoint,
            )
            .on_live::<tau_proto::ToolCancelRequest>(|cx| {
                cancel_call(cx.state, cx.event().target_call_id.clone());
                Ok(())
            })
            .tool_with_group_and_prompt_fragment(
                specs::status_spec(),
                Some(specs::tool_group()),
                None,
                handle_tool,
            )
            .tool_with_group_and_prompt_fragment(
                specs::list_spec(),
                Some(specs::tool_group()),
                None,
                handle_tool,
            )
            .tool_with_group_and_prompt_fragment(
                specs::read_spec(),
                Some(specs::tool_group()),
                None,
                handle_tool,
            )
            .tool_with_group_and_prompt_fragment(
                specs::profile_spec(),
                Some(specs::tool_group()),
                None,
                handle_tool,
            )
            .tool_with_group_and_prompt_fragment(
                specs::post_spec(),
                Some(specs::tool_group()),
                None,
                handle_tool,
            )
            .tool_with_group_and_prompt_fragment(
                specs::react_spec(),
                Some(specs::tool_group()),
                None,
                handle_tool,
            )
            .tool_with_group_and_prompt_fragment(
                specs::follow_spec(),
                Some(specs::tool_group()),
                None,
                handle_tool,
            )
            .tool_with_group_and_prompt_fragment(
                specs::unfollow_spec(),
                Some(specs::tool_group()),
                None,
                handle_tool,
            )
            .tool_with_group_and_prompt_fragment(
                specs::profile_update_spec(),
                Some(specs::tool_group()),
                None,
                handle_tool,
            )
            .tool_with_group_and_prompt_fragment(
                specs::vote_spec(),
                Some(specs::tool_group()),
                None,
                handle_tool,
            )
            .ready_message("Rostra local synchronized view ready");
    }
}

fn configure(
    state: &mut RostraState,
    configure: &tau_proto::Configure,
    config: ExtConfig,
) -> ClientResult<()> {
    if !reconfiguration_allowed(&state.permits) {
        return Err(ClientError::handler(
            "storage_failure: cannot reconfigure while a Rostra database query is active",
        ));
    }
    state.client = None;
    state.identity_secret = None;
    abort_all(&state.running);
    if let Some(task) = state.notifications_task.take() {
        task.abort();
    }
    let mnemonic = configure
        .secrets
        .get(&config.identity_mnemonic_secret)
        .map(tau_proto::SecretValue::expose_secret)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ClientError::handler(
                "invalid_argument: `identity_mnemonic_secret` does not name a supplied nonempty Tau secret",
            )
        })?;
    let identity_secret = RostraIdSecretKey::from_str(mnemonic).map_err(|_| {
        ClientError::handler(
            "invalid_argument: `identity_mnemonic_secret` is not a valid Rostra mnemonic",
        )
    })?;
    let identity = identity_secret.id();
    let state_dir = configure.state_dir.as_ref().ok_or_else(|| {
        ClientError::handler(
            "storage_failure: std-rostra requires persistent extension state; memory-only mode is unsupported",
        )
    })?;
    ensure_private_directory(state_dir).map_err(|_| {
        ClientError::handler("storage_failure: could not create private Rostra state directory")
    })?;
    let database_path = state_dir.join("rostra.redb");
    let permissions_path = database_path.clone();
    let client = state
        .runtime
        .as_ref()
        .expect("runtime exists until state drop")
        .block_on(async {
            let database = Database::open(database_path, identity)
                .await
                .map_err(|_| ())?;
            ensure_private_file(&permissions_path).map_err(|_| ())?;
            Client::builder(identity)
                .db(database)
                .public_mode(false)
                .build()
                .await
                .map_err(|_| ())
        })
        .map_err(|_| {
            ClientError::handler(
                "storage_failure: Rostra database open or client startup failed; check ownership, locking, corruption, and configured identity",
            )
    })?;
    state
        .notifications
        .lock()
        .map_err(|_| ClientError::handler("internal_failure: notification state is unavailable"))?
        .configure(configure.instance_name.clone(), identity, state_dir)
        .map_err(|message| ClientError::handler(format!("storage_failure: {message}")))?;
    state.client = Some(client);
    state.identity_secret = Some(identity_secret);
    state.post_rate_limit = config.post_rate_limit;
    *state
        .post_rate_limit_window
        .lock()
        .expect("post rate-limit state lock") = PostRateLimitWindow::default();
    Ok(())
}

/// Begin reconciliation only after configuration commits successfully.
fn start_notifications(state: &mut RostraState, handle: tau_client::ClientHandle) {
    let Some(client) = state.client.clone() else {
        return;
    };
    state.notifications_task = Some(notifications::spawn(
        state
            .runtime
            .as_ref()
            .expect("runtime exists until state drop"),
        client,
        handle,
        state.notifications.clone(),
        state.notifications_wake.clone(),
    ));
}

/// Keep durable registration and current session membership separate.
fn apply_agent_loaded(
    cx: tau_client::EventContext<'_, RostraState, SessionAgentLoaded>,
) -> ClientResult<()> {
    cx.state
        .notifications
        .lock()
        .map_err(|_| ClientError::handler("internal_failure: notification state is unavailable"))?
        .loaded(cx.event().agent_id.clone());
    cx.state.notifications_wake.notify_one();
    Ok(())
}

/// Stop live delivery on unload without erasing the durable preference.
fn apply_agent_unloaded(
    cx: tau_client::EventContext<'_, RostraState, SessionAgentUnloaded>,
) -> ClientResult<()> {
    cx.state
        .notifications
        .lock()
        .map_err(|_| ClientError::handler("internal_failure: notification state is unavailable"))?
        .unloaded(&cx.event().agent_id);
    cx.state.notifications_wake.notify_one();
    Ok(())
}

/// Opens an agent's notification worker gate after its durable replay
/// completes.
fn apply_agent_replay_complete(
    cx: tau_client::EventContext<'_, RostraState, tau_proto::AgentReplayComplete>,
) -> ClientResult<()> {
    let mut notifications =
        cx.state.notifications.lock().map_err(|_| {
            ClientError::handler("internal_failure: notification state is unavailable")
        })?;
    if cx.event().error.is_none() {
        notifications.replay_complete(cx.event().agent_id.clone());
        cx.state.notifications_wake.notify_one();
    }
    Ok(())
}

/// Advance a receipt checkpoint only from this producer's canonical message
/// echo.
fn apply_notification_checkpoint(
    cx: tau_client::RawEventContext<'_, RostraState>,
) -> ClientResult<()> {
    let Event::MessageDelivered(delivered) = cx.event() else {
        return Ok(());
    };
    if cx.is_replay() {
        return Ok(());
    }
    let mut notifications =
        cx.state.notifications.lock().map_err(|_| {
            ClientError::handler("internal_failure: notification state is unavailable")
        })?;
    notifications
        .acknowledge(delivered)
        .map_err(|message| ClientError::handler(format!("storage_failure: {message}")))?;
    cx.state.notifications_wake.notify_one();
    Ok(())
}

fn reconfiguration_allowed(permits: &Semaphore) -> bool {
    permits.available_permits() == MAX_CONCURRENT_TOOLS
}

fn ensure_private_directory(path: &std::path::Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn ensure_private_file(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn handle_tool(cx: tau_client::ToolContext<'_, RostraState>) -> ClientResult<()> {
    let invoke = cx.invoke().clone();
    let Some(client) = cx.state.client.clone() else {
        let event = tools::tool_error(&invoke, tools::ToolFailure::not_ready());
        let outcome = tau_client::ToolTerminalOutcome::try_from(event)
            .map_err(|_| ClientError::handler("internal_failure: invalid terminal event"))?;
        return cx.handle().report_tool_terminal_detached(outcome);
    };
    let handle = cx.handle();
    let output = handle.clone();
    let permit = match Arc::clone(&cx.state.permits).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            let event = tools::tool_error(&invoke, tools::ToolFailure::capacity());
            let outcome = tau_client::ToolTerminalOutcome::try_from(event)
                .map_err(|_| ClientError::handler("internal_failure: invalid terminal event"))?;
            return handle.report_tool_terminal_detached(outcome);
        }
    };
    let running = Arc::clone(&cx.state.running);
    let identity_secret = cx.state.identity_secret;
    let write_lock = Arc::clone(&cx.state.write_lock);
    let post_rate_limit = cx.state.post_rate_limit;
    let post_rate_limit_window = Arc::clone(&cx.state.post_rate_limit_window);
    let call_id = invoke.call_id.clone();
    let (start_tx, start_rx) = oneshot::channel();
    let worker = cx
        .state
        .runtime
        .as_ref()
        .expect("runtime exists until state drop")
        .spawn(async move {
            if start_rx.await.is_err() {
                return;
            }
            let query_invoke = invoke.clone();
            let query_client = Arc::clone(&client);
            let publication_admitted = Arc::new(AtomicBool::new(false));
            let task_publication_admitted = Arc::clone(&publication_admitted);
            // Dropping a JoinHandle detaches the operation after publication
            // admission. This deliberately precedes Rostra's actual redb call:
            // its effect is unknown once downstream publication begins. Before
            // admission, cancellation aborts this extension task; `unlock_active`
            // can still have made its own lazy-activation side effect.
            let mut task = AbortBeforePublicationAdmission {
                task: tokio::spawn(async move {
                    let _permit = permit;
                    tools::dispatch(
                        &query_invoke,
                        &query_client,
                        identity_secret,
                        write_lock,
                        post_rate_limit,
                        post_rate_limit_window,
                        task_publication_admitted,
                    )
                    .await
                }),
                publication_admitted: Arc::clone(&publication_admitted),
            };
            let event = match tokio::time::timeout(TOOL_DEADLINE, &mut task.task).await {
                Ok(Ok(Ok(text))) => tools::tool_result(&invoke, text),
                Ok(Ok(Err(error))) => tools::tool_error(&invoke, error),
                Ok(Err(_)) => tools::tool_error(&invoke, tools::ToolFailure::internal()),
                Err(_) => tools::tool_error(&invoke, tools::ToolFailure::timeout()),
            };
            let should_report = running
                .lock()
                .is_ok_and(|mut calls| calls.remove(&call_id).is_some());
            if should_report && let Ok(outcome) = tau_client::ToolTerminalOutcome::try_from(event) {
                let _ = output.report_tool_terminal_detached(outcome);
            }
        });
    cx.state
        .running
        .lock()
        .map_err(|_| {
            ClientError::handler("internal_failure: Rostra cancellation registry is unavailable")
        })?
        .insert(
            cx.invoke().call_id.clone(),
            RunningCall {
                abort: worker.abort_handle(),
                tool_name: cx.invoke().tool_name.clone(),
                handle,
            },
        );
    let _ = start_tx.send(());
    Ok(())
}

/// Cancels work before publication admission, but detaches work whose eventual
/// local effect is unknown to the caller.
struct AbortBeforePublicationAdmission<T> {
    /// The tool task that owns the permit and, for writes, the serial lane.
    task: tokio::task::JoinHandle<T>,
    /// Set after activation and immediately before dispatching publication.
    publication_admitted: Arc<AtomicBool>,
}

impl<T> Drop for AbortBeforePublicationAdmission<T> {
    fn drop(&mut self) {
        if !self.publication_admitted.load(Ordering::Acquire) {
            self.task.abort();
        }
    }
}

fn cancel_call(state: &mut RostraState, call_id: tau_proto::ToolCallId) {
    let Some(call) = state
        .running
        .lock()
        .ok()
        .and_then(|mut calls| calls.remove(&call_id))
    else {
        return;
    };
    call.abort.abort();
    let _ = call.handle.report_tool_cancelled_detached(ToolCancelled {
        call_id,
        tool_name: call.tool_name,
        tool_type: tau_proto::ToolType::Function,
    });
}

fn abort_all(running: &Mutex<HashMap<tau_proto::ToolCallId, RunningCall>>) {
    if let Ok(mut calls) = running.lock() {
        calls.drain().for_each(|(_, call)| call.abort.abort());
    }
}

#[cfg(test)]
mod tests;
