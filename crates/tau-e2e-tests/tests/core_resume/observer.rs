//! Replay-aware side UI observer for the spawned Tau daemon.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tau_proto::{
    ClientKind, Event, EventName, EventSelector, GetSessionAgentList, HarnessInputMessage,
    HarnessOutputMessage, Hello, SessionAgentListEntry, SessionAgentListResultPayload,
    SessionAgentListScope, SessionId, Subscribe, UnixMicros,
};
use tau_socket::{SocketPeer, SocketReceive};

const MAX_EVENTS: usize = 4_096;
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: usize = 256 * 1024;

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
    /// Sum of serialized retained event sizes for fail-closed memory bounds.
    event_bytes: usize,
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
        Self::connect_with_prompt_drafts(socket, expected_session, artifact_path, deadline, false)
    }

    /// Connects with the ordinary selectors plus live UI draft observations.
    pub(super) fn connect_observing_prompt_drafts(
        socket: &Path,
        expected_session: &SessionId,
        artifact_path: PathBuf,
        deadline: Instant,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::connect_with_prompt_drafts(socket, expected_session, artifact_path, deadline, true)
    }

    fn connect_with_prompt_drafts(
        socket: &Path,
        expected_session: &SessionId,
        artifact_path: PathBuf,
        deadline: Instant,
        observe_prompt_drafts: bool,
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
                client_name: tau_proto::ExtensionName::parse("tau-e2e-side-observer")
                    .expect("test extension name must satisfy the identifier grammar"),
                client_kind: ClientKind::Ui,
                expected_session_id: None,
                capabilities: Default::default(),
            }))?;
            let selectors = selectors(observe_prompt_drafts);
            peer.send(&HarnessInputMessage::Subscribe(Subscribe {
                historical_selectors: selectors.clone(),
                live_selectors: selectors,
            }))?;
            let mut observer = Self {
                peer,
                events: Vec::new(),
                artifact_path: artifact_path.clone(),
                event_bytes: 0,
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
                        observer.record(observed)?;
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

    /// Waits until the named extension reports readiness.
    pub(super) fn wait_for_extension(
        &mut self,
        name: &str,
        deadline: Instant,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.events.iter().any(|observed| {
            matches!(
                &observed.event,
                Event::ExtensionReady(ready) if ready.extension_name.as_str() == name
            )
        }) {
            return Ok(());
        }
        self.recv_until(deadline, |observed| {
            matches!(
                &observed.event,
                Event::ExtensionReady(ready) if ready.extension_name.as_str() == name
            )
        })?;
        Ok(())
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
            self.record(observed.clone())?;
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
                    })?;
                }
                SocketReceive::Message { .. } => {}
                SocketReceive::Timeout | SocketReceive::Closed => return Ok(()),
            }
        }
    }

    /// Creates S8's durable main with an exact initial lane correlation.
    pub(super) fn create_main(
        &mut self,
        session_id: &SessionId,
        ctx_id: &str,
        prompt: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.peer
            .send(&HarnessInputMessage::emit(Event::UiCreateAgent(
                tau_proto::UiCreateAgent {
                    request_id: "core-resume-observer-create".to_owned(),
                    literal: false,
                    session_id: session_id.clone(),
                    role: "deterministic-main".to_owned(),
                    model_override: None,
                    metadata: Vec::new(),
                    initial_prompt: Some(prompt.to_owned()),
                    message_class: tau_proto::PromptMessageClass::User,
                    originator: tau_proto::PromptOriginator::User,
                    ctx_id: Some(ctx_id.to_owned()),
                    parent_agent: None,
                    ephemeral: false,
                },
            )))?;
        Ok(())
    }

    /// Cancels one exact prompt through this observer's UI connection.
    pub(super) fn cancel_prompt(
        &mut self,
        session_id: &SessionId,
        prompt: &tau_proto::AgentPromptCreated,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.peer
            .send(&HarnessInputMessage::emit(Event::UiCancelPrompt(
                tau_proto::UiCancelPrompt {
                    session_id: session_id.clone(),
                    target_agent_id: Some(prompt.agent_id.clone()),
                    agent_prompt_id: Some(prompt.agent_prompt_id.clone()),
                },
            )))?;
        Ok(())
    }

    /// Queries one authoritative requester-directed roster after replay.
    pub(super) fn roster(
        &mut self,
        session_id: &SessionId,
        scope: SessionAgentListScope,
        deadline: Instant,
    ) -> Result<Vec<SessionAgentListEntry>, Box<dyn std::error::Error>> {
        let request_id = match scope {
            SessionAgentListScope::Current => "core-resume-current",
            SessionAgentListScope::History => "core-resume-history",
        };
        self.peer.send(&HarnessInputMessage::GetSessionAgentList(
            GetSessionAgentList {
                request_id: request_id.to_owned(),
                session_id: session_id.clone(),
                scope,
            },
        ))?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for directed roster".into());
            }
            match self.peer.recv_timeout(remaining)? {
                SocketReceive::Message {
                    message: HarnessOutputMessage::SessionAgentListResult(result),
                } if result.request_id == request_id => {
                    if &result.session_id != session_id {
                        return Err("directed roster returned the wrong session".into());
                    }
                    return match result.result {
                        SessionAgentListResultPayload::Ok { agents } => Ok(agents),
                        SessionAgentListResultPayload::Error { error } => {
                            Err(format!("directed roster failed: {}", error.message).into())
                        }
                    };
                }
                SocketReceive::Message {
                    message: HarnessOutputMessage::Deliver(delivery),
                } => {
                    let (event, replay, recorded_at) = delivery.into_parts();
                    self.record(ObservedEvent {
                        event,
                        replay,
                        recorded_at,
                    })?;
                }
                SocketReceive::Message { .. } => {}
                SocketReceive::Timeout => {
                    return Err("timed out waiting for directed roster".into());
                }
                SocketReceive::Closed => {
                    return Err("observer closed while waiting for directed roster".into());
                }
            }
        }
    }

    /// Ends the sole headless UI connection so Boot A can shut down cleanly.
    pub(super) fn disconnect(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.peer
            .send(&HarnessInputMessage::Disconnect(tau_proto::Disconnect {
                reason: Some("S8 Boot A complete".to_owned()),
            }))?;
        Ok(())
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

    fn record(&mut self, observed: ObservedEvent) -> Result<(), Box<dyn std::error::Error>> {
        let event_bytes = serde_json::to_vec(&observed)?.len();
        if self.events.len() >= MAX_EVENTS
            || self.event_bytes.saturating_add(event_bytes) > MAX_EVENT_BYTES
        {
            return Err("side observer exceeded its event capture bound".into());
        }
        self.event_bytes += event_bytes;
        self.events.push(observed);
        let mut start = self.events.len().saturating_sub(256);
        let bytes = loop {
            let bytes = serde_json::to_vec_pretty(&self.events[start..])?;
            if bytes.len() <= MAX_ARTIFACT_BYTES || start + 1 >= self.events.len() {
                break bytes;
            }
            start += 1;
        };
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err("one side-observer event exceeded the artifact bound".into());
        }
        std::fs::write(&self.artifact_path, bytes)?;
        Ok(())
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

fn selectors(observe_prompt_drafts: bool) -> Vec<EventSelector> {
    use EventName as E;
    [
        E::SESSION_STARTED,
        E::SESSION_AGENT_LOADED,
        E::SESSION_AGENT_UNLOADED,
        E::AGENT_STARTED,
        E::AGENT_MESSAGE_RECEIVED,
        E::AGENT_WATCHES_UPDATED,
        E::AGENT_PROMPT_SUBMITTED,
        E::AGENT_PROMPT_CREATED,
        E::AGENT_PROMPT_STARTED,
        E::AGENT_PROMPT_TERMINATED,
        E::AGENT_INFERENCE_DISPATCH_STARTED,
        E::AGENT_STATS_UPDATED,
        E::PROVIDER_PROMPT_SUBMITTED,
        E::PROVIDER_RESPONSE_FINISHED,
        E::PROVIDER_TOOL_RESULT,
        E::PROVIDER_TOOL_ERROR,
        E::TOOL_REQUEST,
        E::TOOL_STARTED,
        E::TOOL_RESULT_DISPLAY,
        E::TOOL_ERROR,
        E::EXTENSION_READY,
        E::EXTENSION_EXITED,
        E::HARNESS_ROLES_AVAILABLE,
        E::HARNESS_ROLE_SELECTED,
        E::HARNESS_AGENT_CONTEXT_INITIALIZED,
        E::AGENT_REPLAY_COMPLETE,
        E::SESSION_REPLAY_COMPLETE,
        E::HARNESS_NOTICE,
    ]
    .into_iter()
    .chain(observe_prompt_drafts.then_some(E::UI_PROMPT_DRAFT))
    .map(EventSelector::Exact)
    .collect()
}
