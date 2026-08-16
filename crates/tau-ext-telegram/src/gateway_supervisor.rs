//! Supervised Telegram gateway connection lifecycle.

use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tau_proto::{AgentId, NoticeLevel};

use super::gateway_client::{GatewayClient, GatewayClientConfig, GatewaySocketResponse};
use super::{
    ConfigGeneration, Output, ProcessingControl, SharedState, State, emit_gateway_deliveries,
    fail_gateway_client_if_current, retry_gateway_acknowledgements,
};

/// Initial reconnect delay after the gateway is absent or disconnects.
pub(super) const GATEWAY_RECONNECT_INITIAL_DELAY: Duration = Duration::from_millis(100);
/// Maximum low-rate reconnect delay while the gateway remains unavailable.
pub(super) const GATEWAY_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(5);

/// Sole owner of the active gateway connect/reconnect worker.
pub(super) struct GatewaySupervisor {
    /// Join handle for the current configuration worker.
    worker: Mutex<Option<JoinHandle<()>>>,
    /// Unpublished client whose in-flight reannouncement must be cancellable.
    connecting: Arc<Mutex<Option<Arc<GatewayClient>>>>,
    /// One-shot fixture signal emitted immediately before joining the worker.
    #[cfg(test)]
    pub(super) pre_join_observer: Mutex<Option<mpsc::Sender<()>>>,
    /// One-shot fixture signal requiring the typed successful-join token.
    #[cfg(test)]
    pub(super) post_join_observer: Mutex<Option<mpsc::Sender<()>>>,
    /// Optional fixture gate held after the worker joins and before `goodbye`.
    #[cfg(test)]
    pub(super) post_join_gate: Mutex<Option<mpsc::Receiver<()>>>,
    /// Optional fixture gate held by the worker at its retirement boundary.
    #[cfg(test)]
    pub(super) retirement_gate: Mutex<Option<mpsc::Receiver<()>>>,
    /// Fixture-visible proof that the current worker crossed retirement.
    #[cfg(test)]
    worker_retired: Arc<AtomicBool>,
}

impl GatewaySupervisor {
    /// Create an idle supervisor owner.
    pub(super) fn new() -> Self {
        Self {
            worker: Mutex::new(None),
            connecting: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            pre_join_observer: Mutex::new(None),
            #[cfg(test)]
            post_join_observer: Mutex::new(None),
            #[cfg(test)]
            post_join_gate: Mutex::new(None),
            #[cfg(test)]
            retirement_gate: Mutex::new(None),
            #[cfg(test)]
            worker_retired: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start the sole worker for one validated gateway configuration.
    pub(super) fn start(
        &self,
        state: Arc<SharedState>,
        output: Output,
        shutdown: Arc<AtomicBool>,
        gateway_cell: Arc<Mutex<Option<Arc<GatewayClient>>>>,
        config_generation: ConfigGeneration,
        config: GatewayClientConfig,
    ) -> Result<(), String> {
        let connecting = Arc::clone(&self.connecting);
        let worker_context = GatewaySupervisorWorker {
            state,
            output,
            shutdown,
            gateway_cell,
            connecting,
            config_generation,
            config,
        };
        #[cfg(test)]
        let retirement_gate = self
            .retirement_gate
            .lock()
            .expect("gateway retirement gate lock")
            .take();
        #[cfg(test)]
        let worker_retired = {
            self.worker_retired.store(false, Ordering::Relaxed);
            Arc::clone(&self.worker_retired)
        };
        let worker = thread::Builder::new()
            .name("telegram-gateway-supervisor".to_owned())
            .spawn(move || {
                worker_context.run();
                #[cfg(test)]
                if let Some(retirement_gate) = retirement_gate {
                    let _ = retirement_gate.recv();
                }
                #[cfg(test)]
                worker_retired.store(true, Ordering::Release);
            })
            .map_err(|error| format!("starting Telegram gateway supervisor: {error}"))?;
        *self.worker.lock().expect("gateway supervisor lock") = Some(worker);
        Ok(())
    }

    /// Disconnect in-progress clients, join the worker, then release the
    /// published client's gateway lease.
    pub(super) fn stop(
        &self,
        state: &SharedState,
        gateway_cell: &Mutex<Option<Arc<GatewayClient>>>,
    ) {
        if let Some(gateway) = self
            .connecting
            .lock()
            .expect("connecting gateway lock")
            .take()
        {
            gateway.disconnect();
        }
        state.notify_all();
        #[cfg(test)]
        if let Some(observer) = self
            .pre_join_observer
            .lock()
            .expect("gateway pre-join observer lock")
            .take()
        {
            let _ = observer.send(());
        }
        if let Some(worker) = self.worker.lock().expect("gateway supervisor lock").take() {
            let joined = join_gateway_worker(worker);
            #[cfg(test)]
            {
                self.observe_successful_join(joined);
                self.wait_at_post_join_gate();
            }
            #[cfg(not(test))]
            let _ = joined;
        }
        if let Some(gateway) = gateway_cell.lock().expect("gateway lock").take() {
            gateway.goodbye();
        }
    }

    /// Publish fixture evidence that can only consume a successful-join token.
    #[cfg(test)]
    fn observe_successful_join(&self, _joined: JoinedGatewayWorker) {
        assert!(
            self.worker_retired.load(Ordering::Acquire),
            "joined gateway worker must cross retirement before stop returns"
        );
        if let Some(observer) = self
            .post_join_observer
            .lock()
            .expect("gateway post-join observer lock")
            .take()
        {
            let _ = observer.send(());
        }
    }

    /// Wait for a fixture after joining and before releasing the gateway lease.
    #[cfg(test)]
    fn wait_at_post_join_gate(&self) {
        if let Some(gate) = self
            .post_join_gate
            .lock()
            .expect("gateway post-join gate lock")
            .take()
        {
            let _ = gate.recv();
        }
    }
}

/// Proof that the gateway worker joined without panicking.
struct JoinedGatewayWorker;

/// Join one worker and return a token available only on successful completion.
fn join_gateway_worker(worker: JoinHandle<()>) -> JoinedGatewayWorker {
    worker.join().expect("Telegram gateway supervisor panicked");
    JoinedGatewayWorker
}

/// Immutable dependency context owned by one supervisor worker generation.
struct GatewaySupervisorWorker {
    /// Shared desired-route and lifecycle state.
    state: Arc<SharedState>,
    /// Harness output used for bounded notices and delivery reports.
    output: Output,
    /// Process-wide shutdown flag.
    shutdown: Arc<AtomicBool>,
    /// Published connection authority slot.
    gateway_cell: Arc<Mutex<Option<Arc<GatewayClient>>>>,
    /// Unpublished connection attempt slot.
    connecting: Arc<Mutex<Option<Arc<GatewayClient>>>>,
    /// Configuration generation owned by this worker.
    config_generation: ConfigGeneration,
    /// Validated socket configuration.
    config: GatewayClientConfig,
}

/// Desired route snapshot used to make reconnect reannouncement atomic.
#[derive(Clone, Eq, PartialEq)]
struct GatewayRouteSnapshot {
    /// Current session whose routes may be announced.
    session_id: Option<tau_proto::SessionId>,
    /// Sorted agent identities and current display labels.
    agents: Vec<GatewayRoute>,
}

/// One desired agent route and its optional display label.
#[derive(Clone, Eq, PartialEq)]
struct GatewayRoute {
    /// Stable Tau agent identity.
    agent_id: AgentId,
    /// Current human-readable display label.
    display_name: Option<String>,
}

/// Return the current desired gateway routes in deterministic order.
fn gateway_route_snapshot(state: &State) -> GatewayRouteSnapshot {
    let mut agents = state
        .registered_agents
        .iter()
        .map(|agent_id| GatewayRoute {
            agent_id: agent_id.clone(),
            display_name: state.agent_labels.get(agent_id).cloned(),
        })
        .collect::<Vec<_>>();
    agents.sort_by(|left, right| left.agent_id.as_ref().cmp(right.agent_id.as_ref()));
    GatewayRouteSnapshot {
        session_id: state.current_session_id.clone(),
        agents,
    }
}

/// Return whether one worker still owns the active gateway configuration.
fn gateway_supervisor_is_current(state: &State, config_generation: ConfigGeneration) -> bool {
    !state.shutdown_requested
        && state.gateway_config_generation == Some(config_generation)
        && state.config_generation == config_generation
}

/// Return whether a response asks the client to establish a fresh generation.
pub(super) fn gateway_response_requires_reconnect(
    gateway: &GatewayClient,
    response: &GatewaySocketResponse,
) -> bool {
    response.reannounce_required
        || response
            .gateway_generation
            .as_ref()
            .is_some_and(|generation| {
                gateway
                    .generation()
                    .as_ref()
                    .is_none_or(|current| current != generation)
            })
}

/// Advance reconnect backoff without exceeding the low-rate retry bound.
pub(super) fn next_gateway_retry_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(GATEWAY_RECONNECT_MAX_DELAY)
}

/// Run the sole connect/reconnect and heartbeat owner for one configuration.
impl GatewaySupervisorWorker {
    /// Run connect, exact reannouncement, heartbeat, and bounded retry until
    /// cancellation.
    fn run(self) {
        let state = &self.state;
        let output = &self.output;
        let shutdown = &self.shutdown;
        let gateway_cell = &self.gateway_cell;
        let connecting = &self.connecting;
        let config_generation = self.config_generation;
        let config = &self.config;
        let mut retry_delay = GATEWAY_RECONNECT_INITIAL_DELAY;
        let mut outage_reported = false;
        while !shutdown.load(Ordering::Relaxed) && self.is_current() {
            let gateway = Arc::new(GatewayClient::new(config.clone()));
            *connecting.lock().expect("connecting gateway lock") = Some(Arc::clone(&gateway));
            let hello = match gateway.connect_cancellable(|| !self.is_current()) {
                Ok(response) => response,
                Err(message) => {
                    if !outage_reported {
                        output.request_notice(message.to_string(), NoticeLevel::Warning);
                        outage_reported = true;
                    }
                    if !self.wait_for_retry(retry_delay) {
                        break;
                    }
                    retry_delay = next_gateway_retry_delay(retry_delay);
                    continue;
                }
            };
            if !self.is_current() {
                gateway.disconnect();
                self.clear_connecting(&gateway);
                break;
            }
            let mut deliveries = hello.deliveries;
            let mut connection_failed = false;
            let routes = {
                let state = state.lock();
                if !gateway_supervisor_is_current(&state, config_generation) {
                    gateway.disconnect();
                    return;
                }
                gateway_route_snapshot(&state)
            };
            if let Some(session_id) = routes.session_id.as_ref() {
                for route in &routes.agents {
                    if !self.is_current() {
                        gateway.disconnect();
                        self.clear_connecting(&gateway);
                        return;
                    }
                    match gateway.register_agent(
                        session_id.as_ref(),
                        route.agent_id.as_ref(),
                        route.display_name.clone(),
                    ) {
                        Ok(response)
                            if !gateway_response_requires_reconnect(&gateway, &response) =>
                        {
                            deliveries.extend(response.deliveries);
                        }
                        Ok(_) => {
                            connection_failed = true;
                            break;
                        }
                        Err(message) => {
                            if !outage_reported {
                                output.request_notice(message.to_string(), NoticeLevel::Warning);
                                outage_reported = true;
                            }
                            connection_failed = true;
                            break;
                        }
                    }
                }
            }
            if !connection_failed {
                match gateway.complete_reannouncement() {
                    Ok(response) if !gateway_response_requires_reconnect(&gateway, &response) => {
                        deliveries.extend(response.deliveries);
                    }
                    Ok(_) => connection_failed = true,
                    Err(message) => {
                        if !outage_reported {
                            output.request_notice(message.to_string(), NoticeLevel::Warning);
                            outage_reported = true;
                        }
                        connection_failed = true;
                    }
                }
            }
            if !connection_failed {
                let state_guard = state.lock();
                if !gateway_supervisor_is_current(&state_guard, config_generation) {
                    gateway.disconnect();
                    return;
                }
                if gateway_route_snapshot(&state_guard) != routes {
                    connection_failed = true;
                } else {
                    let mut slot = gateway_cell.lock().expect("gateway lock");
                    if !gateway_supervisor_is_current(&state_guard, config_generation) {
                        gateway.disconnect();
                        return;
                    }
                    *slot = Some(Arc::clone(&gateway));
                    self.clear_connecting(&gateway);
                    drop(slot);
                    drop(state_guard);
                    let mut state_guard = state.lock();
                    state_guard.mark_coordination_changed();
                    drop(state_guard);
                    state.notify_all();
                }
            }
            if connection_failed {
                gateway.disconnect();
                self.clear_connecting(&gateway);
                if !self.wait_for_retry(retry_delay) {
                    break;
                }
                retry_delay = next_gateway_retry_delay(retry_delay);
                continue;
            }
            retry_delay = GATEWAY_RECONNECT_INITIAL_DELAY;
            outage_reported = false;
            if emit_gateway_deliveries(
                state,
                output,
                gateway_cell,
                Arc::clone(&gateway),
                deliveries,
            ) == ProcessingControl::Stop
            {
                gateway.disconnect();
                break;
            }
            if !retry_gateway_acknowledgements(state, output, gateway_cell, &gateway) {
                gateway.disconnect();
                if output.check_mandatory_output().is_err() {
                    break;
                }
                if !self.wait_for_retry(retry_delay) {
                    break;
                }
                retry_delay = next_gateway_retry_delay(retry_delay);
                continue;
            }

            loop {
                if !self.wait_for_heartbeat(&gateway, gateway.heartbeat_interval()) {
                    break;
                }
                match gateway.heartbeat() {
                    Ok(response) if !gateway_response_requires_reconnect(&gateway, &response) => {
                        if gateway_cell
                            .lock()
                            .expect("gateway lock")
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(current, &gateway))
                        {
                            if emit_gateway_deliveries(
                                state,
                                output,
                                gateway_cell,
                                Arc::clone(&gateway),
                                response.deliveries,
                            ) == ProcessingControl::Stop
                            {
                                gateway.disconnect();
                                return;
                            }
                            if !retry_gateway_acknowledgements(
                                state,
                                output,
                                gateway_cell,
                                &gateway,
                            ) {
                                if output.check_mandatory_output().is_err() {
                                    gateway.disconnect();
                                    return;
                                }
                                break;
                            }
                        }
                    }
                    Ok(_) => {
                        fail_gateway_client_if_current(gateway_cell, state, &gateway);
                        break;
                    }
                    Err(message) => {
                        if fail_gateway_client_if_current(gateway_cell, state, &gateway)
                            && !outage_reported
                        {
                            output.request_notice(message.to_string(), NoticeLevel::Warning);
                            outage_reported = true;
                        }
                        break;
                    }
                }
            }
            if !self.is_current() {
                break;
            }
            gateway.disconnect();
            self.clear_connecting(&gateway);
            if !self.wait_for_retry(retry_delay) {
                break;
            }
            retry_delay = next_gateway_retry_delay(retry_delay);
        }
    }

    /// Return whether this worker still owns the active configuration.
    fn is_current(&self) -> bool {
        gateway_supervisor_is_current(&self.state.lock(), self.config_generation)
    }

    /// Clear the unpublished client slot only when it still names this attempt.
    fn clear_connecting(&self, gateway: &Arc<GatewayClient>) {
        let mut slot = self.connecting.lock().expect("connecting gateway lock");
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, gateway))
        {
            *slot = None;
        }
    }

    /// Wait for reconnect backoff while remaining configuration-aware.
    fn wait_for_retry(&self, delay: Duration) -> bool {
        let coordination_generation = self.state.lock().coordination_generation;
        let state = self
            .state
            .wait_timeout_while(self.state.lock(), delay, |state| {
                gateway_supervisor_is_current(state, self.config_generation)
                    && state.coordination_generation == coordination_generation
            });
        gateway_supervisor_is_current(&state, self.config_generation)
    }

    /// Wait for heartbeat unless cancellation or retirement wakes the worker.
    fn wait_for_heartbeat(&self, gateway: &Arc<GatewayClient>, delay: Duration) -> bool {
        let state_guard = self
            .state
            .wait_timeout_while(self.state.lock(), delay, |state| {
                gateway_supervisor_is_current(state, self.config_generation)
                    && self
                        .gateway_cell
                        .lock()
                        .expect("gateway lock")
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, gateway))
            });
        gateway_supervisor_is_current(&state_guard, self.config_generation)
            && self
                .gateway_cell
                .lock()
                .expect("gateway lock")
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, gateway))
    }
}
