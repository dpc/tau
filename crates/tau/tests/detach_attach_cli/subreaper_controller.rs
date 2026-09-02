use std::collections::HashMap;
use std::io;
use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{Builder, JoinHandle};
use std::time::Instant;

use rustix_v1::io::Errno;
use rustix_v1::process::{
    Pid, Signal, WaitOptions, WaitStatus, kill_process, kill_process_group, set_child_subreaper,
    wait,
};

use super::{
    BoundedCleanupWorker, ProcessIdentity, SigtermCompletionWorker, group_exists, process_identity,
};

/// Shared status ledger written only by the controller's blocking wait broker.
#[derive(Default)]
struct WaitLedgerState {
    /// Exact identities registered by the controller.
    registered: HashMap<u32, ProcessIdentity>,
    /// Terminal statuses retained even when they precede registration.
    statuses: HashMap<u32, WaitStatus>,
    /// Whether the broker observed final all-child `ECHILD`.
    closed: bool,
    /// First broker invariant or syscall failure.
    error: Option<String>,
}

/// Cloneable query endpoint for the broker's retained status ledger.
#[derive(Clone)]
struct WaitBrokerHandle {
    /// Shared ledger and wake condition.
    shared: Arc<(Mutex<WaitLedgerState>, Condvar)>,
}

impl WaitBrokerHandle {
    /// Registers one committed PID/start identity without requiring current
    /// procfs presence or discarding an already-recorded status.
    fn register(&self, identity: ProcessIdentity) -> io::Result<()> {
        let (state, wake) = &*self.shared;
        let mut state = state.lock().expect("wait broker ledger lock");
        if let Some(existing) = state.registered.insert(identity.pid, identity)
            && existing != identity
        {
            return Err(io::Error::other(format!(
                "wait broker PID {} identity changed from {} to {}",
                identity.pid, existing.start_time, identity.start_time
            )));
        }
        if state.closed && !state.statuses.contains_key(&identity.pid) {
            return Err(io::Error::other(format!(
                "wait broker closed before registered child {}:{} became terminal",
                identity.pid, identity.start_time
            )));
        }
        wake.notify_all();
        Ok(())
    }

    /// Waits for one exact registered PID's retained terminal status.
    fn status_until(&self, pid: u32, deadline: Instant) -> io::Result<Option<WaitStatus>> {
        let (state, wake) = &*self.shared;
        let mut state = state.lock().expect("wait broker ledger lock");
        loop {
            if let Some(error) = &state.error {
                return Err(io::Error::other(error.clone()));
            }
            if let Some(status) = state.statuses.get(&pid) {
                return Ok(Some(*status));
            }
            if state.closed {
                return Err(io::Error::other(format!(
                    "wait broker reached ECHILD without registered PID {pid}"
                )));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let (next, timeout) = wake
                .wait_timeout(state, remaining)
                .expect("wait broker ledger wait");
            state = next;
            if timeout.timed_out() {
                return Ok(None);
            }
        }
    }

    /// Requires all registered sentinels to be terminal before final `ECHILD`.
    fn closed_until(&self, deadline: Instant) -> io::Result<()> {
        let (state, wake) = &*self.shared;
        let mut state = state.lock().expect("wait broker ledger lock");
        loop {
            if let Some(error) = &state.error {
                return Err(io::Error::other(error.clone()));
            }
            if state.closed {
                let missing = state
                    .registered
                    .values()
                    .filter(|identity| !state.statuses.contains_key(&identity.pid))
                    .map(ProcessIdentity::encode)
                    .collect::<Vec<_>>();
                if missing.is_empty() {
                    return Ok(());
                }
                return Err(io::Error::other(format!(
                    "wait broker reached ECHILD before registered sentinels: {}",
                    missing.join(",")
                )));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "wait broker did not reach final ECHILD",
                ));
            }
            let (next, timeout) = wake
                .wait_timeout(state, remaining)
                .expect("wait broker ledger wait");
            state = next;
            if timeout.timed_out() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "wait broker did not reach final ECHILD",
                ));
            }
        }
    }
}

/// Sole controller-local owner of every direct and adopted child wait status.
struct WaitBroker {
    /// Shared query endpoint used by the SIGTERM completion action.
    handle: WaitBrokerHandle,
    /// Exact direct worker identity registered before the broker starts.
    worker: ProcessIdentity,
    /// Exact watchdog identity once READY commits it.
    watchdog: Option<ProcessIdentity>,
    /// Exact Tau-group leader identity used only for failure cleanup.
    group_anchor: Option<ProcessIdentity>,
    /// Existing non-renewable controller lifecycle deadline.
    deadline: Instant,
    /// Blocking all-child waiter, joined only after final `ECHILD`.
    waiter: Option<JoinHandle<()>>,
}

impl WaitBroker {
    /// Starts the sole blocking `waitpid(any child)` owner immediately after
    /// direct-worker spawn, with that worker already registered.
    fn spawn(worker: ProcessIdentity, deadline: Instant) -> io::Result<Self> {
        let shared = Arc::new((Mutex::new(WaitLedgerState::default()), Condvar::new()));
        let handle = WaitBrokerHandle {
            shared: Arc::clone(&shared),
        };
        handle.register(worker)?;
        let waiter = Builder::new()
            .name("owned-process-wait-broker".into())
            .spawn(move || run_wait_broker(shared))?;
        Ok(Self {
            handle,
            worker,
            watchdog: None,
            group_anchor: None,
            deadline,
            waiter: Some(waiter),
        })
    }

    /// Registers READY's exact watchdog identity and retains it for cleanup.
    fn register_watchdog(&mut self, identity: ProcessIdentity) -> io::Result<()> {
        self.handle.register(identity)?;
        self.watchdog = Some(identity);
        Ok(())
    }

    /// Retains the exact Tau-group leader for identity-safe failure cleanup.
    fn register_group_anchor(&mut self, identity: ProcessIdentity) {
        self.group_anchor = Some(identity);
    }

    /// Returns the ledger endpoint used by the shared SIGTERM action.
    fn handle(&self) -> WaitBrokerHandle {
        self.handle.clone()
    }

    /// Requires exact sentinel statuses and final all-child `ECHILD`, then
    /// joins the sole waiter.
    fn finish(&mut self, worker_expected_signal: Signal, stderr: &[u8]) -> io::Result<()> {
        let worker_status = self
            .handle
            .status_until(self.worker.pid, self.deadline)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "worker terminal status"))?;
        if worker_status.terminating_signal() != Some(worker_expected_signal.as_raw()) {
            return Err(status_error(
                "cleanup worker",
                self.worker,
                worker_status,
                stderr,
            ));
        }
        let watchdog = self
            .watchdog
            .ok_or_else(|| io::Error::other("watchdog identity was not registered"))?;
        let watchdog_status = self
            .handle
            .status_until(watchdog.pid, self.deadline)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "watchdog terminal status"))?;
        if watchdog_status.exit_status() != Some(0) {
            return Err(status_error(
                "cleanup watchdog",
                watchdog,
                watchdog_status,
                stderr,
            ));
        }
        self.handle.closed_until(self.deadline)?;
        self.join_waiter();
        Ok(())
    }

    /// Requires clean worker and watcher exits plus final `ECHILD` for the
    /// deliberate pre-registration abort subcase.
    fn finish_prearm_abort(&mut self, watchdog: ProcessIdentity, stderr: &[u8]) -> io::Result<()> {
        let worker_status = self
            .handle
            .status_until(self.worker.pid, self.deadline)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "aborted worker status"))?;
        if worker_status.exit_status() != Some(0) {
            return Err(status_error(
                "aborted controlled worker",
                self.worker,
                worker_status,
                stderr,
            ));
        }
        let watchdog_status = self
            .handle
            .status_until(watchdog.pid, self.deadline)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "aborted watcher status"))?;
        if watchdog_status.exit_status() != Some(0) {
            return Err(status_error(
                "aborted controlled watcher",
                watchdog,
                watchdog_status,
                stderr,
            ));
        }
        self.handle.closed_until(self.deadline)?;
        self.join_waiter();
        Ok(())
    }

    /// Joins the waiter only after the ledger has recorded final `ECHILD`.
    fn join_waiter(&mut self) {
        if let Some(waiter) = self.waiter.take() {
            waiter.join().expect("wait broker thread joins");
        }
    }

    /// Signals only still-matching committed identities so the sole waiter can
    /// drain every child on an error path.
    fn signal_owned_children(&self) {
        signal_identity(self.worker, Signal::KILL);
        if let Some(watchdog) = self.watchdog {
            signal_identity(watchdog, Signal::KILL);
        }
        if let Some(anchor) = self.group_anchor
            && process_identity(anchor.pid) == Some(anchor)
            && let Some(pgid) = Pid::from_raw(i32::try_from(anchor.pid).unwrap_or(i32::MAX))
        {
            let _ = kill_process_group(pgid, Signal::KILL);
        }
    }
}

impl Drop for WaitBroker {
    fn drop(&mut self) {
        if self.waiter.is_none() {
            return;
        }
        self.signal_owned_children();
        if self.handle.closed_until(self.deadline).is_ok() {
            self.join_waiter();
        }
    }
}

/// Worker pipes paired with the sole wait broker instead of `Child::wait`.
pub(super) struct BrokeredCleanupWorker {
    /// Drops first so failure cleanup drains child statuses before pipe
    /// readers.
    broker: WaitBroker,
    /// Direct worker pipes and retained stdin barrier; its `Child` is detached.
    worker: BoundedCleanupWorker,
}

impl BrokeredCleanupWorker {
    /// Spawns the worker, starts the broker immediately, then relinquishes the
    /// competing `Child` wait handle.
    pub(super) fn spawn(command: std::process::Command, deadline: Instant) -> io::Result<Self> {
        let mut worker = BoundedCleanupWorker::spawn(command)?;
        let identity = process_identity(worker.id())
            .ok_or_else(|| io::Error::other("capture direct cleanup worker identity"))?;
        let broker = WaitBroker::spawn(identity, deadline)?;
        let child = worker
            .release_child_wait_handle()
            .expect("direct worker wait handle remains owned");
        drop(child);
        Ok(Self { broker, worker })
    }

    /// Registers exact READY identities before any success-path signal.
    pub(super) fn register_readiness(
        &mut self,
        watchdog: ProcessIdentity,
        group_anchor: ProcessIdentity,
    ) -> io::Result<()> {
        self.broker.register_watchdog(watchdog)?;
        self.broker.register_group_anchor(group_anchor);
        Ok(())
    }

    /// Requires worker SIGTERM, watchdog success, and final all-child `ECHILD`.
    pub(super) fn finish_broker(&mut self, stderr: &[u8]) -> io::Result<()> {
        self.broker.finish(Signal::TERM, stderr)
    }

    /// Requires clean pre-arm teardown and final all-child `ECHILD`.
    pub(super) fn finish_prearm_abort(
        &mut self,
        watchdog: ProcessIdentity,
        stderr: &[u8],
    ) -> io::Result<()> {
        self.broker.finish_prearm_abort(watchdog, stderr)
    }

    /// Acknowledges registration while retaining the stdin failure barrier.
    pub(super) fn write_stdin(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.worker.write_stdin(bytes)
    }

    /// Intentionally aborts only the synthetic pre-arm registration path.
    pub(super) fn close_stdin(&mut self) {
        self.worker.close_stdin();
    }

    /// Returns the broker-retained direct worker status.
    pub(super) fn wait_until(&mut self, deadline: Instant) -> io::Result<Option<ExitStatus>> {
        self.broker
            .handle()
            .status_until(self.worker.id(), deadline)
            .map(|status| status.map(|status| ExitStatus::from_raw(status.as_raw())))
    }

    /// Collects inherited diagnostic stderr through terminal EOF.
    pub(super) fn collect_stderr_until(&mut self, deadline: Instant) -> io::Result<Vec<u8>> {
        self.worker.collect_stderr_until(deadline)
    }

    /// Receives one shared stdout line before the lifecycle deadline.
    pub(super) fn recv_line_until(
        &self,
        deadline: Instant,
        boundary: &str,
    ) -> io::Result<Option<String>> {
        self.worker.recv_line_until(deadline, boundary)
    }
}

impl SigtermCompletionWorker for BrokeredCleanupWorker {
    fn id(&self) -> u32 {
        self.worker.id()
    }

    fn recv_line_until(&self, deadline: Instant, boundary: &str) -> io::Result<Option<String>> {
        self.worker.recv_line_until(deadline, boundary)
    }

    fn wait_until(&mut self, deadline: Instant) -> io::Result<Option<ExitStatus>> {
        self.broker
            .handle()
            .status_until(self.worker.id(), deadline)
            .map(|status| status.map(|status| ExitStatus::from_raw(status.as_raw())))
    }

    fn collect_stderr_until(&mut self, deadline: Instant) -> io::Result<Vec<u8>> {
        self.worker.collect_stderr_until(deadline)
    }
}

/// Enables process-global child adoption only inside a dedicated controller.
pub(super) fn set_isolated_child_subreaper() -> io::Result<()> {
    set_child_subreaper(Pid::from_raw(1))?;
    Ok(())
}

/// Runs the sole wait owner until final all-child `ECHILD`.
fn run_wait_broker(shared: Arc<(Mutex<WaitLedgerState>, Condvar)>) {
    loop {
        let event = wait(WaitOptions::empty());
        let (state, wake) = &*shared;
        let mut state = state.lock().expect("wait broker ledger lock");
        match event {
            Ok(Some((pid, status))) => {
                let pid =
                    u32::try_from(pid.as_raw_nonzero().get()).expect("waited PID is positive");
                if state.statuses.insert(pid, status).is_some() {
                    state.error = Some(format!("wait broker recorded duplicate PID {pid}"));
                }
            }
            Ok(None) => {
                state.error =
                    Some("blocking waitpid(any child) returned without a status".to_owned());
            }
            Err(Errno::CHILD) => {
                let missing = state
                    .registered
                    .values()
                    .filter(|identity| !state.statuses.contains_key(&identity.pid))
                    .map(ProcessIdentity::encode)
                    .collect::<Vec<_>>();
                if !missing.is_empty() {
                    state.error = Some(format!(
                        "wait broker reached early ECHILD before registered sentinels: {}",
                        missing.join(",")
                    ));
                }
                state.closed = true;
            }
            Err(error) => state.error = Some(format!("wait broker failed: {error}")),
        }
        let terminal = state.closed || state.error.is_some();
        wake.notify_all();
        drop(state);
        if terminal {
            return;
        }
    }
}

/// Formats a role-specific nonzero status with all captured inherited stderr.
fn status_error(
    role: &str,
    identity: ProcessIdentity,
    status: WaitStatus,
    stderr: &[u8],
) -> io::Error {
    io::Error::other(format!(
        "{role} {}:{} exited unsuccessfully: {status:?}\nstderr:\n{}",
        identity.pid,
        identity.start_time,
        String::from_utf8_lossy(stderr)
    ))
}

/// Sends one cleanup signal only while the committed PID/start still matches.
fn signal_identity(identity: ProcessIdentity, signal: Signal) {
    if process_identity(identity.pid) != Some(identity) {
        return;
    }
    if let Some(pid) = Pid::from_raw(i32::try_from(identity.pid).unwrap_or(i32::MAX)) {
        let _ = kill_process(pid, signal);
    }
}

/// Proves a group-filtered `ECHILD` can precede future adoption without
/// affecting the sole all-child broker.
pub(super) fn require_group_filtered_echild(pgid: u32) -> io::Result<()> {
    let pgid = Pid::from_raw(i32::try_from(pgid).expect("controlled PGID fits i32"))
        .expect("positive controlled PGID");
    match rustix_v1::process::waitpgid(pgid, WaitOptions::NOHANG) {
        Err(Errno::CHILD) => Ok(()),
        Ok(Some((pid, status))) => Err(io::Error::other(format!(
            "group-filtered probe stole child {} with {status:?}",
            pid.as_raw_nonzero()
        ))),
        Ok(None) => Err(io::Error::other(
            "target group was already a direct child before future adoption",
        )),
        Err(error) => Err(error.into()),
    }
}

/// Confirms no process remains in the exact controlled group after broker
/// completion.
pub(super) fn require_group_absent(pgid: u32) -> io::Result<()> {
    if group_exists(pgid)? {
        Err(io::Error::other(format!(
            "controlled process group {pgid} survived broker completion"
        )))
    } else {
        Ok(())
    }
}
