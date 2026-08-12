//! Authenticated fixture-private release worker for deterministic dummy calls.

#[cfg(test)]
mod tests;

use std::io::{ErrorKind, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{fs, thread};

use tau_client::{ClientError, ClientResult};
use tau_proto::{ToolError, ToolResult, ToolType};

use super::{HOLD_READY_TIMEOUT, HOLD_TERMINAL_TIMEOUT, HOLD_TERMINAL_TIMEOUT_SECS};

/// Maximum accepted release frame, including its newline delimiter.
const RELEASE_FRAME_MAX_BYTES: usize = 4096;
/// Maximum clients admitted to the serial frame reader.
const PENDING_CONNECTION_LIMIT: usize = 8;
/// Maximum time a serial reader lets one incomplete client occupy the worker.
const CLIENT_READ_TIMEOUT: Duration = Duration::from_millis(100);

/// Validated configuration for one authenticated release socket.
#[derive(Clone, Debug)]
pub(super) struct ReleaseConfig {
    /// Fresh socket leaf supplied by the fixture caller.
    socket_path: PathBuf,
    /// Nonempty authentication nonce supplied by the fixture caller.
    nonce: String,
}

impl ReleaseConfig {
    /// Validates one fixture-private release endpoint.
    pub(super) fn new(socket_path: PathBuf, nonce: String) -> ClientResult<Self> {
        if nonce.is_empty() {
            return Err(ClientError::handler(
                "hold_until_success_release requires a non-empty release_nonce",
            ));
        }
        Ok(Self { socket_path, nonce })
    }
}

/// One authenticated release worker owned by the extension state.
pub(super) struct ReleaseHold {
    /// Correlation identity accepted by this worker.
    call_id: tau_proto::ToolCallId,
    /// Lifecycle control channel.
    signal: mpsc::Sender<WorkerInput>,
    /// Worker joined on cancellation, disconnect, or state teardown.
    join: thread::JoinHandle<()>,
    /// Exactly one terminal path may own publication.
    terminal: Arc<Mutex<TerminalOwner>>,
}

/// Honest terminal ownership for the release lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalOwner {
    /// No terminal path has won.
    Unclaimed,
    /// An authenticated client won.
    Release,
    /// Correlated cancellation won.
    Cancellation,
    /// Worker timeout or listener failure won.
    Worker,
    /// Extension shutdown won without synthetic output.
    Shutdown,
}

/// Release-worker-only inputs.
enum WorkerInput {
    /// Permit an already authenticated release after readiness publication.
    Arm,
    /// Emit one correlated cancellation terminal.
    Cancel,
    /// Exit without terminal output.
    Shutdown,
    /// One bounded accepted client.
    Connection(UnixStream),
    /// The listener failed.
    AcceptError(std::io::Error),
}

/// Typed logical release frame accepted by the private socket.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseFrame {
    /// Invocation correlation identity.
    call_id: tau_proto::ToolCallId,
    /// Fixture-generated authentication value.
    release_nonce: String,
}

/// Removes the fixture-private socket when its worker exits.
struct SocketGuard {
    /// Socket leaf removed when listener ownership ends.
    path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != ErrorKind::NotFound
        {
            eprintln!(
                "tau-ext-test-dummy: failed to remove release socket {}: {error}",
                self.path.display()
            );
        }
    }
}

impl ReleaseHold {
    /// Starts one listener/worker pair and waits until both can accept control.
    pub(super) fn start(
        config: ReleaseConfig,
        invoke: tau_proto::ToolStarted,
        terminals: super::TerminalSender,
    ) -> ClientResult<Self> {
        let listener = UnixListener::bind(&config.socket_path).map_err(|error| {
            ClientError::handler(format!("failed to bind release socket: {error}"))
        })?;
        let call_id = invoke.call_id.clone();
        let terminal = Arc::new(Mutex::new(TerminalOwner::Unclaimed));
        let worker_terminal = Arc::clone(&terminal);
        let (signal, inputs) = mpsc::channel();
        let listener_input = signal.clone();
        let (ready, readiness) = mpsc::channel();
        let socket_path = config.socket_path.clone();
        let join = thread::spawn(move || {
            ReleaseWorker {
                listener,
                socket_path,
                nonce: config.nonce,
                invoke,
                terminals,
                terminal: worker_terminal,
                input: listener_input,
                inputs,
                ready,
            }
            .run();
        });
        if let Err(error) = readiness.recv_timeout(HOLD_READY_TIMEOUT) {
            let _ = signal.send(WorkerInput::Shutdown);
            let _ = join.join();
            return Err(ClientError::handler(format!(
                "hold_until_success_release worker did not become ready: {error}"
            )));
        }
        Ok(Self {
            call_id,
            signal,
            join,
            terminal,
        })
    }

    /// Returns this worker's correlated call id.
    pub(super) fn call_id(&self) -> &tau_proto::ToolCallId {
        &self.call_id
    }

    /// Arms authenticated release after readiness has been published.
    pub(super) fn arm(&self) {
        let _ = self.signal.send(WorkerInput::Arm);
    }

    /// Claims cancellation and signals the worker.
    pub(super) fn cancel(&self) {
        let _ = self.signal.send(WorkerInput::Cancel);
    }

    /// Claims shutdown and joins without synthetic output.
    pub(super) fn shutdown(self) {
        if claim(&self.terminal, TerminalOwner::Shutdown) {
            let _ = self.signal.send(WorkerInput::Shutdown);
        }
        let _ = self.join.join();
    }

    /// Joins a naturally completed worker.
    pub(super) fn join(self) -> ClientResult<()> {
        self.join
            .join()
            .map_err(|_| ClientError::handler("deterministic release worker panicked"))
    }
}

/// Owns all resources for one fixed-thread release arbitration loop.
struct ReleaseWorker {
    /// Bound fixture-private listener transferred from startup.
    listener: UnixListener,
    /// Socket leaf used to wake the acceptor and clean up on exit.
    socket_path: PathBuf,
    /// Per-invocation authentication secret.
    nonce: String,
    /// Canonical invocation identity and result metadata.
    invoke: tau_proto::ToolStarted,
    /// Worker-to-loop terminal publication adapter.
    terminals: super::TerminalSender,
    /// Shared exactly-one terminal arbitration state.
    terminal: Arc<Mutex<TerminalOwner>>,
    /// Acceptor-to-worker and lifecycle-input sender.
    input: mpsc::Sender<WorkerInput>,
    /// Serialized connection and lifecycle inputs.
    inputs: mpsc::Receiver<WorkerInput>,
    /// Startup acknowledgement sent after the acceptor exists.
    ready: mpsc::Sender<()>,
}

impl ReleaseWorker {
    /// Runs until one terminal owner wins or shutdown closes the fixture.
    fn run(self) {
        let Self {
            listener,
            socket_path,
            nonce,
            invoke,
            terminals,
            terminal,
            input,
            inputs,
            ready,
        } = self;
        let _socket_guard = SocketGuard {
            path: socket_path.clone(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let accept_stop = Arc::clone(&stop);
        let pending = Arc::new(AtomicUsize::new(0));
        let accept_pending = Arc::clone(&pending);
        let acceptor = thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if accept_stop.load(Ordering::Acquire) {
                            return;
                        }
                        if reserve_connection(&accept_pending)
                            && input.send(WorkerInput::Connection(stream)).is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = input.send(WorkerInput::AcceptError(error));
                        return;
                    }
                }
            }
        });
        if ready.send(()).is_err() {
            stop.store(true, Ordering::Release);
            let _ = UnixStream::connect(&socket_path);
            let _ = acceptor.join();
            return;
        }

        let deadline = Instant::now() + HOLD_TERMINAL_TIMEOUT;
        let mut armed = false;
        let mut authenticated = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let next = if remaining.is_zero() {
                Err(mpsc::RecvTimeoutError::Timeout)
            } else {
                inputs.recv_timeout(remaining)
            };
            match next {
                Ok(WorkerInput::Arm) => armed = true,
                Ok(WorkerInput::Cancel) => {
                    if claim(&terminal, TerminalOwner::Cancellation) {
                        terminals.send(
                            tau_proto::ToolCancelled {
                                presentation: Default::default(),
                                call_id: invoke.call_id.clone(),
                                tool_name: invoke.tool_name.clone(),
                                tool_type: ToolType::Function,
                            }
                            .into(),
                        );
                    }
                    break;
                }
                Ok(WorkerInput::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(WorkerInput::Connection(mut stream)) => {
                    pending.fetch_sub(1, Ordering::AcqRel);
                    let timeout = remaining.min(CLIENT_READ_TIMEOUT);
                    if let Err(error) = configure_client_timeout(&stream, timeout) {
                        if claim(&terminal, TerminalOwner::Worker) {
                            terminals.send(
                                worker_error(
                                    &invoke,
                                    format!("failed to bound release client read: {error}"),
                                )
                                .into(),
                            );
                        }
                        break;
                    }
                    authenticated |= read_release_frame(&mut stream).is_some_and(|release| {
                        release.call_id == invoke.call_id && release.release_nonce == nonce
                    });
                }
                Ok(WorkerInput::AcceptError(error)) => {
                    if claim(&terminal, TerminalOwner::Worker) {
                        terminals.send(
                            worker_error(&invoke, format!("release socket accept failed: {error}"))
                                .into(),
                        );
                    }
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if claim(&terminal, TerminalOwner::Worker) {
                        terminals.send(worker_error(
                        &invoke,
                        format!(
                            "deterministic hold reached its {HOLD_TERMINAL_TIMEOUT_SECS} second deadline"
                        ),
                    ).into());
                    }
                    break;
                }
            }
            if armed && authenticated && claim(&terminal, TerminalOwner::Release) {
                terminals.send(
                    ToolResult {
                        presentation: Default::default(),
                        call_id: invoke.call_id.clone(),
                        tool_name: invoke.tool_name.clone(),
                        tool_type: ToolType::Function,
                        result: tau_proto::CborValue::Text("restart succeeded".to_owned()),
                        provider_content: Vec::new(),
                        kind: tau_proto::ToolResultKind::Final,
                        originator: invoke.originator.clone(),
                        display: None,
                    }
                    .into(),
                );
                break;
            }
        }
        drop(inputs);
        stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&socket_path);
        let _ = acceptor.join();
    }
}

/// Applies the per-client read deadline and exposes setup failure to
/// arbitration.
fn configure_client_timeout(stream: &UnixStream, timeout: Duration) -> std::io::Result<()> {
    stream.set_read_timeout(Some(timeout))
}

/// Reserves one slot without permitting the accepted-client queue to grow.
fn reserve_connection(pending: &AtomicUsize) -> bool {
    let mut count = pending.load(Ordering::Acquire);
    loop {
        if PENDING_CONNECTION_LIMIT <= count {
            return false;
        }
        match pending.compare_exchange_weak(count, count + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => count = observed,
        }
    }
}

/// Claims exactly one terminal owner.
fn claim(terminal: &Mutex<TerminalOwner>, owner: TerminalOwner) -> bool {
    let mut current = terminal.lock().expect("release terminal lock");
    if *current != TerminalOwner::Unclaimed {
        return false;
    }
    *current = owner;
    true
}

/// Builds one worker-owned terminal error.
fn worker_error(invoke: &tau_proto::ToolStarted, message: String) -> ToolError {
    ToolError {
        presentation: Default::default(),
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        tool_type: ToolType::Function,
        message,
        details: None,
        originator: invoke.originator.clone(),
        display: None,
    }
}

/// Reads one exactly bounded newline-terminated frame without waiting for EOF.
fn read_release_frame(stream: &mut impl Read) -> Option<ReleaseFrame> {
    let mut bytes = Vec::with_capacity(RELEASE_FRAME_MAX_BYTES);
    let mut byte = [0];
    while bytes.len() < RELEASE_FRAME_MAX_BYTES {
        match stream.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) => {
                bytes.push(byte[0]);
                if byte[0] == b'\n' {
                    bytes.pop();
                    return serde_json::from_slice(&bytes).ok();
                }
            }
            Err(_) => return None,
        }
    }
    None
}
