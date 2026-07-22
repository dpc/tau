//! Replay-aware socket observer for the session-restore scenarios.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tau_proto::{
    AgentId, AgentRuntimeState, ClientKind, Event, EventName, EventSelector, GetSessionAgentList,
    HarnessInputMessage, HarnessOutputMessage, Hello, SessionAgentListEntry,
    SessionAgentListResultPayload, SessionAgentListScope, SessionId, Subscribe,
};
use tau_socket::{SocketPeer, SocketReceive};

use super::{SESSION, provider_response_contains};

/// Ordered replay-aware delivery retained by the session-restore observer.
#[derive(Clone, Debug)]
pub(super) struct Observed {
    /// Typed event payload.
    pub(super) event: Event,
    /// Whether this delivery came from historical replay.
    pub(super) replay: bool,
    /// Durable append timestamp when replayed from a store.
    pub(super) recorded_at: Option<tau_proto::UnixMicros>,
}

/// Replay-aware socket observer with requester-directed roster support.
pub(super) struct SessionRestoreObserver {
    /// Connected same-user UI peer.
    pub(super) peer: SocketPeer,
    /// Ordered exact-selector deliveries.
    pub(super) events: Vec<Observed>,
}

impl SessionRestoreObserver {
    /// Connects and installs the exact session-restore historical/live selector
    /// set.
    pub(super) fn connect(socket: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut peer = loop {
            match SocketPeer::connect(socket) {
                Ok(peer) => break peer,
                Err(_) if Instant::now() < deadline => std::thread::yield_now(),
                Err(error) => return Err(error.into()),
            }
        };
        peer.send(&HarnessInputMessage::Hello(Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "tau-e2e-session-restore".into(),
            client_kind: ClientKind::Ui,
            capabilities: Default::default(),
        }))?;
        let selectors = selectors();
        peer.send(&HarnessInputMessage::Subscribe(Subscribe {
            historical_selectors: selectors.clone(),
            live_selectors: selectors,
        }))?;
        Ok(Self {
            peer,
            events: Vec::new(),
        })
    }

    /// Creates the durable main with the explicit deterministic main role.
    pub(super) fn create_main(
        &mut self,
        ctx_id: &str,
        prompt: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.peer
            .send(&HarnessInputMessage::emit(Event::UiCreateAgent(
                tau_proto::UiCreateAgent {
                    session_id: SESSION.into(),
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

    /// Creates one memory-only worker without dispatching an initial prompt and
    /// returns its harness-minted identity after exact membership publication.
    pub(super) fn create_ephemeral_worker(
        &mut self,
        parent_agent: &AgentId,
    ) -> Result<AgentId, Box<dyn std::error::Error>> {
        let start = self.events.len();
        self.peer
            .send(&HarnessInputMessage::emit(Event::UiCreateAgent(
                tau_proto::UiCreateAgent {
                    session_id: SESSION.into(),
                    role: "deterministic-worker".to_owned(),
                    model_override: None,
                    metadata: Vec::new(),
                    initial_prompt: None,
                    message_class: tau_proto::PromptMessageClass::User,
                    originator: tau_proto::PromptOriginator::User,
                    ctx_id: None,
                    parent_agent: Some(parent_agent.clone()),
                    ephemeral: true,
                },
            )))?;
        self.wait_for_ephemeral_worker(parent_agent, start)
    }

    /// Waits for one exact ephemeral worker creation and membership after
    /// `start`, returning its harness-minted identity.
    fn wait_for_ephemeral_worker(
        &mut self,
        parent_agent: &AgentId,
        start: usize,
    ) -> Result<AgentId, Box<dyn std::error::Error>> {
        let mut next = start;
        let mut agent_id = None;
        let mut loaded = false;
        loop {
            while let Some(observed) = self.events.get(next) {
                next += 1;
                match &observed.event {
                    Event::AgentStarted(started)
                        if started.ephemeral
                            && started.role == "deterministic-worker"
                            && started.parent_agent.as_ref() == Some(parent_agent) =>
                    {
                        if let Some(previous) = agent_id.replace(started.agent_id.clone()) {
                            return Err(format!(
                                "multiple ephemeral worker creation facts observed: \
                                 {previous} then {} (replay={})",
                                started.agent_id, observed.replay
                            )
                            .into());
                        }
                    }
                    Event::SessionAgentLoaded(event)
                        if event.ephemeral
                            && agent_id
                                .as_ref()
                                .is_some_and(|agent_id| agent_id == &event.agent_id) =>
                    {
                        loaded = true;
                    }
                    _ => {}
                }
                if loaded {
                    return agent_id.ok_or_else(|| "ephemeral worker identity missing".into());
                }
            }
            self.recv_one()?;
        }
    }

    /// Submits one exact direct user prompt to a restored agent route.
    pub(super) fn submit(
        &mut self,
        agent_id: &AgentId,
        ctx_id: &str,
        text: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.peer
            .send(&HarnessInputMessage::emit(Event::UiPromptSubmitted(
                tau_proto::UiPromptSubmitted {
                    session_id: SESSION.into(),
                    text: text.to_owned(),
                    agent_id: agent_id.clone(),
                    message_class: tau_proto::PromptMessageClass::User,
                    originator: tau_proto::PromptOriginator::User,
                    ctx_id: Some(ctx_id.to_owned()),
                },
            )))?;
        Ok(())
    }

    /// Submits a direct prompt and requires the exact unknown-agent rejection
    /// without any accepted prompt fact for that identity.
    pub(super) fn assert_absent_route(
        &mut self,
        agent_id: &AgentId,
        ctx_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let start = self.events.len();
        self.submit(agent_id, ctx_id, "this route must not exist")?;
        let expected = format!("unknown agent `{agent_id}`");
        self.recv_until(|observed| {
            matches!(&observed.event, Event::HarnessNotice(notice) if notice.message == expected)
        })?;
        if self.events[start..].iter().any(|observed| {
            matches!(
                &observed.event,
                Event::AgentPromptSubmitted(prompt) if &prompt.agent_id == agent_id
            ) || matches!(
                &observed.event,
                Event::AgentPromptCreated(prompt) if &prompt.agent_id == agent_id
            )
        }) {
            return Err(format!("absent agent route accepted a prompt for {agent_id}").into());
        }
        Ok(())
    }

    /// Waits for one exact assistant terminal marker.
    pub(super) fn wait_for_marker(
        &mut self,
        marker: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.recv_until(|observed| {
            matches!(
                &observed.event,
                Event::ProviderResponseFinished(finished)
                    if provider_response_contains(finished, marker)
            )
        })
        .map(|_| ())
    }

    /// Waits for one exact agent-owned assistant marker, including deliveries
    /// already retained after the supplied event index.
    pub(super) fn wait_for_agent_marker(
        &mut self,
        agent_id: &AgentId,
        marker: &str,
        start: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut next = start;
        loop {
            while let Some(observed) = self.events.get(next) {
                next += 1;
                if matches!(
                    &observed.event,
                    Event::ProviderResponseFinished(finished)
                        if &finished.agent_id == agent_id
                            && provider_response_contains(finished, marker)
                ) {
                    return Ok(());
                }
            }
            self.recv_one()?;
        }
    }

    /// Waits for one post-index idle/no-tools fact for the exact agent.
    pub(super) fn wait_for_agent_idle_after(
        &mut self,
        agent_id: &AgentId,
        start: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut next = start;
        loop {
            while let Some(observed) = self.events.get(next) {
                next += 1;
                if matches!(
                    &observed.event,
                    Event::AgentStatsUpdated(stats)
                        if &stats.agent_id == agent_id
                            && stats.runtime_state == AgentRuntimeState::Idle
                            && stats.tools.in_flight == 0
                ) {
                    return Ok(());
                }
            }
            self.recv_one()?;
        }
    }

    /// Waits until two distinct agents' latest complete stats are idle and
    /// empty.
    pub(super) fn wait_for_two_idle_agents(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut latest = BTreeMap::new();
        for observed in &self.events {
            if let Event::AgentStatsUpdated(stats) = &observed.event {
                latest.insert(
                    stats.agent_id.clone(),
                    (stats.runtime_state, stats.tools.in_flight),
                );
            }
        }
        loop {
            if latest.len() == 2
                && latest
                    .values()
                    .all(|(state, in_flight)| *state == AgentRuntimeState::Idle && *in_flight == 0)
            {
                return Ok(());
            }
            let observed = self.recv_one()?;
            if let Event::AgentStatsUpdated(stats) = observed.event {
                latest.insert(stats.agent_id, (stats.runtime_state, stats.tools.in_flight));
            }
        }
    }

    /// Waits for the non-replay session catch-up boundary.
    pub(super) fn wait_for_session_boundary(
        &mut self,
        session_id: &SessionId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.events.iter().any(|observed| {
            !observed.replay
                && observed.recorded_at.is_none()
                && matches!(
                    &observed.event,
                    Event::SessionReplayComplete(done)
                        if &done.session_id == session_id && done.error.is_none()
                )
        }) {
            return Ok(());
        }
        self.recv_until(|observed| {
            !observed.replay
                && observed.recorded_at.is_none()
                && matches!(
                    &observed.event,
                    Event::SessionReplayComplete(done)
                        if &done.session_id == session_id && done.error.is_none()
                )
        })
        .map(|_| ())
    }

    /// Queries one authoritative directed roster scope while retaining
    /// deliveries.
    pub(super) fn roster(
        &mut self,
        session_id: &SessionId,
        scope: SessionAgentListScope,
    ) -> Result<Vec<SessionAgentListEntry>, Box<dyn std::error::Error>> {
        let request_id = match scope {
            SessionAgentListScope::Current => "session-restore-current",
            SessionAgentListScope::History => "session-restore-history",
        };
        self.peer.send(&HarnessInputMessage::GetSessionAgentList(
            GetSessionAgentList {
                request_id: request_id.to_owned(),
                session_id: session_id.clone(),
                scope,
            },
        ))?;
        loop {
            match self.recv_output()? {
                HarnessOutputMessage::SessionAgentListResult(result)
                    if result.request_id == request_id =>
                {
                    if &result.session_id != session_id {
                        return Err(format!(
                            "roster response session mismatch: requested {session_id}, got {}",
                            result.session_id
                        )
                        .into());
                    }
                    return match result.result {
                        SessionAgentListResultPayload::Ok { agents } => Ok(agents),
                        SessionAgentListResultPayload::Error { error } => Err(format!(
                            "roster failed: {:?}: {}",
                            error.kind, error.message
                        )
                        .into()),
                    };
                }
                HarnessOutputMessage::Deliver(delivery) => {
                    let (event, replay, recorded_at) = delivery.into_parts();
                    self.events.push(Observed {
                        event,
                        replay,
                        recorded_at,
                    });
                }
                _ => {}
            }
        }
    }

    /// Receives and retains events until one predicate matches.
    pub(super) fn recv_until(
        &mut self,
        mut predicate: impl FnMut(&Observed) -> bool,
    ) -> Result<Observed, Box<dyn std::error::Error>> {
        loop {
            let observed = self.recv_one()?;
            if predicate(&observed) {
                return Ok(observed);
            }
        }
    }

    /// Receives and retains one semantic delivery.
    pub(super) fn recv_one(&mut self) -> Result<Observed, Box<dyn std::error::Error>> {
        loop {
            match self.recv_output()? {
                HarnessOutputMessage::Deliver(delivery) => {
                    let (event, replay, recorded_at) = delivery.into_parts();
                    let observed = Observed {
                        event,
                        replay,
                        recorded_at,
                    };
                    self.events.push(observed.clone());
                    return Ok(observed);
                }
                HarnessOutputMessage::Disconnect(disconnect) => {
                    return Err(disconnect
                        .reason
                        .unwrap_or_else(|| "session restore observer disconnected".to_owned())
                        .into());
                }
                _ => {}
            }
        }
    }

    /// Receives one protocol output under the fixed test deadline.
    pub(super) fn recv_output(
        &mut self,
    ) -> Result<HarnessOutputMessage, Box<dyn std::error::Error>> {
        match self.peer.recv_timeout(Duration::from_secs(15))? {
            SocketReceive::Message { message } => Ok(message),
            SocketReceive::Timeout => Err("timed out waiting for session restore event".into()),
            SocketReceive::Closed => Err("session restore observer socket closed".into()),
        }
    }
}

/// Returns the exact selector set needed for session-restore lifecycle and
/// replay assertions.
fn selectors() -> Vec<EventSelector> {
    use EventName as E;
    [
        E::SESSION_STARTED,
        E::AGENT_STARTED,
        E::SESSION_AGENT_LOADED,
        E::SESSION_AGENT_UNLOADED,
        E::AGENT_PROMPT_SUBMITTED,
        E::AGENT_PROMPT_CREATED,
        E::AGENT_INFERENCE_DISPATCH_STARTED,
        E::PROVIDER_PROMPT_SUBMITTED,
        E::PROVIDER_RESPONSE_FINISHED,
        E::TOOL_REQUEST,
        E::TOOL_STARTED,
        E::TOOL_RESULT,
        E::PROVIDER_TOOL_RESULT,
        E::AGENT_STATS_UPDATED,
        E::AGENT_WATCHES_UPDATED,
        E::AGENT_MESSAGE_RECEIVED,
        E::HARNESS_NOTICE,
        E::AGENT_REPLAY_COMPLETE,
        E::SESSION_REPLAY_COMPLETE,
    ]
    .into_iter()
    .map(EventSelector::Exact)
    .collect()
}
