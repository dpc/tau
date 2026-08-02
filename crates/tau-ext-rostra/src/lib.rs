//! Relay-only, read-only Rostra tools backed by an extension-owned database.
//!
//! `ARCH-tau-ext-rostra` records the persistent-state, synchronization, and
//! hostile-content boundaries implemented here.

mod cursor;
mod projection;
mod specs;
mod tools;

use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::future::Future;
use std::io::{Read, Write};
use std::str::FromStr as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rostra_client::{Client, Database, RostraId};
use tau_client::{ClientError, ClientResult, ExtensionBuilder, TauExtension};
use tau_proto::ToolCancelled;
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::sync::{Semaphore, oneshot};
use tokio::task::AbortHandle;

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

const TOOL_DEADLINE: Duration = Duration::from_secs(10);
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
        runtime: Some(runtime),
        running: Arc::new(Mutex::new(HashMap::new())),
        permits: Arc::new(Semaphore::new(MAX_CONCURRENT_TOOLS)),
    };
    tau_client::TauExtensionRunner::new(RostraExtension)
        .run_detached_writer(reader, writer, state)?;
    Ok(())
}

/// Strict public configuration for one Rostra identity.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtConfig {
    /// Public Rostra identity whose synchronized local view is exposed.
    identity: String,
}

/// Runtime state owned by one extension process.
struct RostraState {
    /// Full client; declared before the runtime so shutdown drops it first.
    client: Option<Arc<Client>>,
    /// Executor that owns Rostra and bounded tool tasks.
    runtime: Option<tokio::runtime::Runtime>,
    /// Abort handles for calls that can still produce a terminal result.
    running: Arc<Mutex<HashMap<tau_proto::ToolCallId, RunningCall>>>,
    /// Process-wide bound on concurrent tool work.
    permits: Arc<Semaphore>,
}

impl Drop for RostraState {
    fn drop(&mut self) {
        abort_all(&self.running);
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
                |cx| configure(cx.state, cx.configure, cx.config),
                |cx| {
                    cx.state.client = None;
                    abort_all(&cx.state.running);
                },
            )
            .on_live::<tau_proto::ToolCancelRequest>(|cx| {
                cancel_call(cx.state, cx.event().target_call_id.clone());
                Ok(())
            })
            .tool(specs::status_spec(), handle_tool)
            .tool(specs::list_spec(), handle_tool)
            .tool(specs::read_spec(), handle_tool)
            .tool(specs::profile_spec(), handle_tool)
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
    abort_all(&state.running);
    let identity = RostraId::from_str(&config.identity)
        .map_err(|_| ClientError::handler("invalid_argument: `identity` is not a Rostra id"))?;
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
    state.client = Some(client);
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
            let mut task = AbortOnDrop(cx_spawn(async move {
                let _permit = permit;
                tools::dispatch(&query_invoke, &query_client).await
            }));
            let event = match tokio::time::timeout(TOOL_DEADLINE, &mut task.0).await {
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

fn cx_spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(future)
}

/// Aborts the async wrapper and suppresses late terminals; an upstream redb
/// read may remain.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
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
