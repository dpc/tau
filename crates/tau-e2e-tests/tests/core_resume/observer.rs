//! Replay-aware side UI observer for the spawned Tau daemon.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tau_proto::{
    ClientKind, Event, EventName, EventSelector, HarnessInputMessage, HarnessOutputMessage, Hello,
    SessionId, Subscribe, UnixMicros,
};
use tau_socket::{SocketPeer, SocketReceive};

/// One delivered semantic event with replay and append-time metadata preserved.
#[derive(Clone, Debug, Serialize)]
pub(super) struct ObservedEvent {
    /// Delivered event payload.
    pub event: Event,
    /// Whether this was historical catch-up.
    pub replay: bool,
    /// Durable append time when available.
    pub recorded_at: Option<UnixMicros>,
}

/// Side observer that intentionally sees both transcript and replay boundaries.
pub(super) struct SideObserver {
    /// Connected UI protocol peer.
    peer: SocketPeer,
    /// Ordered deliveries retained for exact assertions and diagnostics.
    pub events: Vec<ObservedEvent>,
    /// Continuously refreshed bounded observer artifact.
    artifact_path: PathBuf,
}

/// Minimal typed projection of one private daemon discovery record.
#[derive(Debug, Deserialize)]
struct RuntimeMetadata {
    /// Session advertised by this private daemon.
    session_id: SessionId,
}

impl SideObserver {
    /// Connects to a fully initialized daemon and installs exact selectors.
    pub(super) fn connect(
        socket: &Path,
        expected_session: &SessionId,
        artifact_path: PathBuf,
        deadline: Instant,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        loop {
            let mut peer = match SocketPeer::connect(socket) {
                Ok(peer) => peer,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    thread::yield_now();
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            peer.send(&HarnessInputMessage::Hello(Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION,
                client_name: "tau-e2e-side-observer".into(),
                client_kind: ClientKind::Ui,
                capabilities: Default::default(),
            }))?;
            let selectors = selectors();
            peer.send(&HarnessInputMessage::Subscribe(Subscribe {
                historical_selectors: selectors.clone(),
                live_selectors: selectors,
            }))?;
            let mut observer = Self {
                peer,
                events: Vec::new(),
                artifact_path: artifact_path.clone(),
            };
            let attempt_deadline = deadline.min(Instant::now() + Duration::from_secs(2));
            while Instant::now() < attempt_deadline {
                match observer.recv_one(attempt_deadline) {
                    Ok(observed) => {
                        let initialized = matches!(
                            &observed.event,
                            Event::SessionStarted(started)
                                if &started.session_id == expected_session
                        );
                        observer.record(observed);
                        if initialized {
                            return Ok(observer);
                        }
                    }
                    Err(_) => break,
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "side observer never received SessionStarted for `{expected_session}`"
                )
                .into());
            }
        }
    }

    /// Receives until `predicate` accepts one delivery, retaining every prior
    /// event in order.
    pub(super) fn recv_until(
        &mut self,
        deadline: Instant,
        mut predicate: impl FnMut(&ObservedEvent) -> bool,
    ) -> Result<ObservedEvent, Box<dyn std::error::Error>> {
        loop {
            let observed = self.recv_one(deadline).map_err(|error| {
                format!("{error}; last observed events:\n{}", self.event_tail())
            })?;
            let matched = predicate(&observed);
            self.record(observed.clone());
            if matched {
                return Ok(observed);
            }
        }
    }

    /// Drains immediately available deliveries without discarding metadata.
    pub(super) fn drain_available(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            match self.peer.recv_timeout(Duration::ZERO)? {
                SocketReceive::Message {
                    message: HarnessOutputMessage::Deliver(delivery),
                } => {
                    let (event, replay, recorded_at) = delivery.into_parts();
                    self.record(ObservedEvent {
                        event,
                        replay,
                        recorded_at,
                    });
                }
                SocketReceive::Message { .. } => {}
                SocketReceive::Timeout | SocketReceive::Closed => return Ok(()),
            }
        }
    }

    fn recv_one(&mut self, deadline: Instant) -> Result<ObservedEvent, Box<dyn std::error::Error>> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for side-observer event".into());
            }
            match self.peer.recv_timeout(remaining)? {
                SocketReceive::Message {
                    message: HarnessOutputMessage::Deliver(delivery),
                } => {
                    let (event, replay, recorded_at) = delivery.into_parts();
                    return Ok(ObservedEvent {
                        event,
                        replay,
                        recorded_at,
                    });
                }
                SocketReceive::Message {
                    message: HarnessOutputMessage::Disconnect(disconnect),
                } => {
                    return Err(disconnect
                        .reason
                        .unwrap_or_else(|| "side observer disconnected".to_owned())
                        .into());
                }
                SocketReceive::Message { .. } => {}
                SocketReceive::Timeout => {
                    return Err("timed out waiting for side-observer event".into());
                }
                SocketReceive::Closed => return Err("side-observer socket closed".into()),
            }
        }
    }

    fn record(&mut self, observed: ObservedEvent) {
        self.events.push(observed);
        let start = self.events.len().saturating_sub(256);
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.events[start..]) {
            let _ = std::fs::write(&self.artifact_path, bytes);
        }
    }

    fn event_tail(&self) -> String {
        self.events
            .iter()
            .rev()
            .take(16)
            .map(|observed| {
                format!(
                    "replay={} recorded_at={:?} event={:?}",
                    observed.replay, observed.recorded_at, observed.event
                )
            })
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Discovers the sole private daemon socket and typed session id.
pub(super) fn discover_daemon(
    runtime_root: &Path,
    expected_session: Option<&SessionId>,
    deadline: Instant,
) -> Result<(PathBuf, SessionId), Box<dyn std::error::Error>> {
    let harnesses = runtime_root.join("tau/harnesses");
    loop {
        let mut matches = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&harnesses) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                let Ok(metadata) = serde_json::from_slice::<RuntimeMetadata>(&bytes) else {
                    continue;
                };
                if expected_session.is_none_or(|expected| expected == &metadata.session_id) {
                    let socket = path.with_extension("sock");
                    if socket.exists() {
                        matches.push((socket, metadata.session_id));
                    }
                }
            }
        }
        if matches.len() == 1 {
            return Ok(matches.remove(0));
        }
        if matches.len() > 1 {
            return Err("multiple private Tau daemons discovered".into());
        }
        if Instant::now() >= deadline {
            return Err("timed out discovering private Tau daemon".into());
        }
        thread::yield_now();
    }
}

fn selectors() -> Vec<EventSelector> {
    use EventName as E;
    [
        E::SESSION_STARTED,
        E::SESSION_AGENT_LOADED,
        E::AGENT_STARTED,
        E::AGENT_PROMPT_SUBMITTED,
        E::AGENT_PROMPT_STARTED,
        E::AGENT_PROMPT_TERMINATED,
        E::AGENT_STATS_UPDATED,
        E::PROVIDER_RESPONSE_FINISHED,
        E::PROVIDER_TOOL_RESULT,
        E::PROVIDER_TOOL_ERROR,
        E::TOOL_REQUEST,
        E::TOOL_STARTED,
        E::TOOL_RESULT,
        E::TOOL_ERROR,
        E::EXTENSION_READY,
        E::EXTENSION_EXITED,
        E::AGENT_REPLAY_COMPLETE,
        E::SESSION_REPLAY_COMPLETE,
        E::HARNESS_NOTICE,
    ]
    .into_iter()
    .map(EventSelector::Exact)
    .collect()
}
