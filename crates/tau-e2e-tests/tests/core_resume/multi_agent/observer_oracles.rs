//! Protocol, replay, roster, and provider-budget oracles for S8.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use tau_proto::{
    AgentId, AgentNavigationMode, AgentRuntimeState, ContextItem, Event, SessionAgentFacts,
    SessionAgentLifecycle, SessionAgentListEntry, SessionAgentPersistence, SessionId, ToolCallId,
};

use super::{Identities, ProviderTurns, agent_start_projection};
use crate::observer::{ObservedEvent, SideObserver};
use crate::provider_finished_contains;

impl Identities {
    /// Extracts the sole main and worker IDs from observed creation facts.
    pub(super) fn from_events(
        events: &[ObservedEvent],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut main = None;
        let mut worker = None;
        for observed in events {
            if let Event::AgentStarted(started) = &observed.event {
                match started.role.as_str() {
                    "deterministic-main" => main = Some(started.agent_id.clone()),
                    "deterministic-worker" => worker = Some(started.agent_id.clone()),
                    role => return Err(format!("unexpected S8 role `{role}`").into()),
                }
            }
        }
        Ok(Self {
            main: main.ok_or("S8 main identity missing")?,
            worker: worker.ok_or("S8 worker identity missing")?,
        })
    }
}

/// Waits for one terminal provider response containing an exact marker.
pub(super) fn wait_marker(
    observer: &mut SideObserver,
    marker: &str,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    observer
        .recv_until(deadline, |observed| {
            provider_finished_contains(&observed.event, marker)
        })
        .map(|_| ())
}

/// Waits for one live terminal marker owned by the exact agent.
pub(super) fn wait_agent_marker(
    observer: &mut SideObserver,
    agent_id: &AgentId,
    marker: &str,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    observer
        .recv_until(deadline, |observed| {
            !observed.replay
                && matches!(
                    &observed.event,
                    Event::ProviderResponseFinished(finished)
                        if &finished.agent_id == agent_id
                            && provider_finished_contains(&observed.event, marker)
                )
        })
        .map(|_| ())
}

/// Waits for one live idle/no-tools fact for the exact agent.
pub(super) fn wait_agent_idle(
    observer: &mut SideObserver,
    agent_id: &AgentId,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    observer
        .recv_until(deadline, |observed| {
            !observed.replay
                && matches!(
                    &observed.event,
                    Event::AgentStatsUpdated(stats)
                        if &stats.agent_id == agent_id
                            && stats.runtime_state == AgentRuntimeState::Idle
                            && stats.tools.in_flight == 0
                )
        })
        .map(|_| ())
}

/// Waits until the latest complete stats show exactly two idle agents.
pub(super) fn wait_two_idle(
    observer: &mut SideObserver,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let latest = observer.events.iter().fold(
            std::collections::BTreeMap::new(),
            |mut latest, observed| {
                if let Event::AgentStatsUpdated(stats) = &observed.event {
                    latest.insert(
                        stats.agent_id.clone(),
                        (stats.runtime_state, stats.tools.in_flight),
                    );
                }
                latest
            },
        );
        if latest.len() == 2
            && latest
                .values()
                .all(|(state, tools)| *state == AgentRuntimeState::Idle && *tools == 0)
        {
            return Ok(());
        }
        observer.recv_until(deadline, |_| true)?;
    }
}

/// Checks Boot A's exact durable main/worker creation and membership facts.
pub(super) fn assert_boot_a(
    events: &[ObservedEvent],
    session_id: &SessionId,
    identities: &Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::AgentStarted(started) if !observed.replay => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    if started.len() != 2
        || started
            .iter()
            .map(|event| event.agent_id.clone())
            .collect::<BTreeSet<_>>()
            != identities.all().into_iter().cloned().collect()
    {
        return Err("S8 Boot A did not create exactly the stable main/worker pair".into());
    }
    let worker = started
        .iter()
        .find(|event| event.agent_id == identities.worker)
        .ok_or("S8 worker creation missing")?;
    if worker.parent_agent.as_ref() != Some(&identities.main)
        || worker.display_name.as_deref() != Some("deterministic worker")
    {
        return Err("S8 worker creation facts changed".into());
    }
    for agent_id in identities.all() {
        let loads = events
            .iter()
            .filter(|observed| {
                matches!(
                    &observed.event,
                    Event::SessionAgentLoaded(loaded)
                        if &loaded.session_id == session_id
                            && &loaded.agent_id == agent_id
                            && !loaded.ephemeral
                )
            })
            .count();
        if loads != 1 {
            return Err(format!("S8 Boot A load count for {agent_id} was {loads}").into());
        }
    }
    Ok(())
}

/// Waits for and validates both agent boundaries before the session boundary.
pub(super) fn wait_resume_boundaries(
    observer: &mut SideObserver,
    session_id: &SessionId,
    identities: &Identities,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    observer.recv_until(deadline, |observed| {
        !observed.replay
            && observed.recorded_at.is_none()
            && matches!(
                &observed.event,
                Event::SessionReplayComplete(done)
                    if &done.session_id == session_id && done.error.is_none()
            )
    })?;
    let session_boundary = observer
        .events
        .iter()
        .position(|observed| {
            !observed.replay
                && observed.recorded_at.is_none()
                && matches!(
                    &observed.event,
                    Event::SessionReplayComplete(done) if &done.session_id == session_id
                )
        })
        .ok_or("S8 session replay boundary missing")?;
    for agent_id in identities.all() {
        let boundaries = observer
            .events
            .iter()
            .enumerate()
            .filter(|(_, observed)| {
                !observed.replay
                    && observed.recorded_at.is_none()
                    && matches!(
                        &observed.event,
                        Event::AgentReplayComplete(done)
                            if &done.agent_id == agent_id
                                && done.session_id.as_ref() == Some(session_id)
                                && done.error.is_none()
                    )
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if boundaries
            .as_slice()
            .first()
            .is_none_or(|index| *index >= session_boundary)
            || boundaries.len() != 1
        {
            return Err(
                format!("S8 replay boundary mismatch for {agent_id}: {boundaries:?}").into(),
            );
        }
        if events_after_agent_boundary(
            &observer.events,
            *boundaries.first().expect("one boundary"),
            agent_id,
        ) {
            return Err(format!("S8 delivered replay for {agent_id} after its boundary").into());
        }
    }
    Ok(())
}

/// Proves pre-input Boot B delivery is replay-only and dispatch-free.
pub(super) fn assert_replay_only_before_input(
    events: &[ObservedEvent],
    session_id: &SessionId,
    identities: &Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let resume = events
        .iter()
        .filter(|observed| {
            matches!(
                &observed.event,
                Event::SessionStarted(started)
                    if &started.session_id == session_id
                        && started.reason == tau_proto::SessionStartReason::Resume
            )
        })
        .count();
    if resume != 1 {
        return Err(format!("S8 observed {resume} resume starts").into());
    }
    assert_replayed_agent_start(events, identities)?;
    let replayed_loads = events
        .iter()
        .filter_map(|observed| {
            (observed.replay && observed.recorded_at.is_some())
                .then_some(&observed.event)
                .and_then(|event| match event {
                    Event::SessionAgentLoaded(loaded) => Some(loaded.agent_id.clone()),
                    _ => None,
                })
        })
        .collect::<BTreeSet<_>>();
    if replayed_loads != identities.all().into_iter().cloned().collect()
        || events
            .iter()
            .any(|observed| matches!(observed.event, Event::SessionAgentUnloaded(_)))
    {
        return Err("S8 replayed membership was not the exact loaded main/worker pair".into());
    }
    for (agent_id, marker) in [
        (&identities.main, "worker completion observed"),
        (&identities.worker, "worker boot-a complete"),
    ] {
        let terminals = events
            .iter()
            .filter(|observed| {
                observed.replay
                    && observed.recorded_at.is_some()
                    && matches!(
                        &observed.event,
                        Event::ProviderResponseFinished(finished)
                            if &finished.agent_id == agent_id
                                && provider_finished_contains(&observed.event, marker)
                    )
            })
            .count();
        if terminals != 1 {
            return Err(format!("S8 replay terminal count for {agent_id} was {terminals}").into());
        }
    }
    if events.iter().any(|observed| {
        !observed.replay
            && matches!(
                observed.event,
                Event::AgentPromptCreated(_)
                    | Event::ProviderPromptSubmitted(_)
                    | Event::AgentInferenceDispatchStarted(_)
            )
    }) {
        return Err("S8 replay dispatched unintended live provider work".into());
    }
    Ok(())
}

/// Rechecks exact replay, boundary, and membership cardinality after both
/// directed roster replies have drained every pre-input delivery.
pub(super) fn assert_final_pre_input_replay(
    events: &[ObservedEvent],
    session_id: &SessionId,
    identities: &Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let session_boundaries = events
        .iter()
        .enumerate()
        .filter_map(|(index, observed)| {
            matches!(
                &observed.event,
                Event::SessionReplayComplete(done)
                    if !observed.replay
                        && observed.recorded_at.is_none()
                        && &done.session_id == session_id
                        && done.error.is_none()
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let [session_boundary] = session_boundaries.as_slice() else {
        return Err(format!(
            "S8 final session boundary cardinality changed: {session_boundaries:?}"
        )
        .into());
    };
    if events
        .iter()
        .skip(session_boundary + 1)
        .any(|observed| observed.replay || observed.recorded_at.is_some())
    {
        return Err("S8 delivered historical replay after the session boundary".into());
    }
    let all_session_boundaries = events
        .iter()
        .filter(|observed| matches!(observed.event, Event::SessionReplayComplete(_)))
        .count();
    if all_session_boundaries != 1 {
        return Err(format!(
            "S8 observed {all_session_boundaries} total session replay boundaries"
        )
        .into());
    }
    let mut seen_agent_boundaries = BTreeSet::new();
    let mut agent_boundaries = BTreeMap::new();
    for (index, observed) in events.iter().enumerate() {
        if let Event::AgentReplayComplete(done) = &observed.event
            && (observed.replay
                || observed.recorded_at.is_some()
                || done.session_id.as_ref() != Some(session_id)
                || done.error.is_some()
                || !identities.all().contains(&&done.agent_id)
                || !seen_agent_boundaries.insert(done.agent_id.clone())
                || index >= *session_boundary
                || events_after_agent_boundary(events, index, &done.agent_id))
        {
            return Err(format!("S8 invalid agent replay boundary: {observed:?}").into());
        }
        if let Event::AgentReplayComplete(done) = &observed.event {
            agent_boundaries.insert(done.agent_id.clone(), index);
        }
    }
    if seen_agent_boundaries != identities.all().into_iter().cloned().collect() {
        return Err(
            format!("S8 final agent boundary set changed: {seen_agent_boundaries:?}").into(),
        );
    }
    assert_replay_ownership(events, identities, &agent_boundaries)?;
    let membership = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::SessionAgentLoaded(loaded) => Some((observed, loaded)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let replayed_loads = membership
        .iter()
        .map(|(_, loaded)| loaded.agent_id.clone())
        .collect::<Vec<_>>();
    let unloads = events
        .iter()
        .filter(|observed| matches!(observed.event, Event::SessionAgentUnloaded(_)))
        .collect::<Vec<_>>();
    if !unloads.is_empty()
        || membership.len() != 2
        || membership.iter().any(|(observed, loaded)| {
            !observed.replay
                || observed.recorded_at.is_none()
                || &loaded.session_id != session_id
                || loaded.ephemeral
        })
        || replayed_loads.iter().cloned().collect::<BTreeSet<_>>()
            != identities.all().into_iter().cloned().collect()
    {
        return Err(format!(
            "S8 final replay membership changed: loads={replayed_loads:?}, \
                 unloads={unloads:?}"
        )
        .into());
    }
    Ok(())
}

fn assert_replay_ownership(
    events: &[ObservedEvent],
    identities: &Identities,
    boundaries: &BTreeMap<AgentId, usize>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut prompt_owners = BTreeMap::new();
    let mut call_owners = BTreeMap::new();
    for observed in events {
        match &observed.event {
            Event::AgentPromptCreated(value) => {
                if prompt_owners
                    .insert(value.agent_prompt_id.clone(), value.agent_id.clone())
                    .is_some()
                {
                    return Err(format!(
                        "S8 duplicate replay prompt identity: {}",
                        value.agent_prompt_id
                    )
                    .into());
                }
            }
            Event::ToolRequest(value)
                if call_owners
                    .insert(value.call_id.clone(), value.agent_id.clone())
                    .is_some() =>
            {
                return Err(format!("S8 duplicate replay tool identity: {}", value.call_id).into());
            }
            _ => {}
        }
    }
    for (index, observed) in events.iter().enumerate() {
        let owner = replay_event_owner(&observed.event, &prompt_owners, &call_owners)?;
        let Some(owner) = owner else {
            continue;
        };
        if !identities.all().contains(&&owner) {
            return Err(
                format!("S8 replay fact has unexpected owner {owner}: {observed:?}").into(),
            );
        }
        let boundary = boundaries
            .get(&owner)
            .ok_or_else(|| format!("S8 replay owner {owner} has no boundary"))?;
        if !observed.replay || observed.recorded_at.is_none() || index >= *boundary {
            return Err(format!(
                "S8 agent fact did not precede {owner}'s boundary with historical metadata: \
                 {observed:?}"
            )
            .into());
        }
    }
    Ok(())
}

fn replay_event_owner(
    event: &Event,
    prompt_owners: &BTreeMap<tau_proto::AgentPromptId, AgentId>,
    call_owners: &BTreeMap<ToolCallId, AgentId>,
) -> Result<Option<AgentId>, Box<dyn std::error::Error>> {
    let direct = match event {
        Event::AgentStarted(value) => Some(value.agent_id.clone()),
        Event::AgentPromptSubmitted(value) => Some(value.agent_id.clone()),
        Event::AgentPromptCreated(value) => Some(value.agent_id.clone()),
        Event::AgentPromptStarted(value) => Some(value.agent_id.clone()),
        Event::AgentPromptTerminated(value) => Some(value.agent_id.clone()),
        Event::AgentInferenceDispatchStarted(value) => Some(value.agent_id.clone()),
        Event::AgentStatsUpdated(value) => Some(value.agent_id.clone()),
        Event::AgentWatchesUpdated(value) => Some(value.watcher_id.clone()),
        Event::ProviderResponseFinished(value) => Some(value.agent_id.clone()),
        Event::ToolRequest(value) => Some(value.agent_id.clone()),
        Event::ToolStarted(value) => Some(value.agent_id.clone()),
        Event::AgentMessageReceived(value) => Some(value.recipient_id.clone()),
        Event::ProviderPromptSubmitted(value) => Some(
            prompt_owners
                .get(&value.agent_prompt_id)
                .ok_or_else(|| {
                    format!(
                        "S8 provider prompt has unknown replay identity {}",
                        value.agent_prompt_id
                    )
                })?
                .clone(),
        ),
        Event::ProviderToolResult(value) => Some(tool_owner(call_owners, &value.call_id)?),
        Event::ProviderToolError(value) => Some(tool_owner(call_owners, &value.call_id)?),
        Event::ToolResultDisplay(value) => Some(tool_owner(call_owners, &value.call_id)?),
        Event::ToolError(value) => Some(tool_owner(call_owners, &value.call_id)?),
        _ => None,
    };
    Ok(direct)
}

fn tool_owner(
    owners: &BTreeMap<ToolCallId, AgentId>,
    call_id: &ToolCallId,
) -> Result<AgentId, Box<dyn std::error::Error>> {
    owners
        .get(call_id)
        .cloned()
        .ok_or_else(|| format!("S8 tool fact has unknown replay call id {call_id}").into())
}

fn assert_replayed_agent_start(
    events: &[ObservedEvent],
    identities: &Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let call_id = ToolCallId::from("s8-agent-start");
    let requests = replay_positions(events, |event| {
        matches!(
            event,
            Event::ToolRequest(value)
                if value.call_id == call_id
                    && value.agent_id == identities.main
                    && value.tool_name.as_str() == "agent_start"
                    && agent_start_projection::arguments_match(&value.arguments)
        )
    });
    let starts = replay_positions(events, |event| {
        matches!(
            event,
            Event::ToolStarted(value)
                if value.call_id == call_id
                    && value.agent_id == identities.main
                    && value.tool_name.as_str() == "agent_start"
                    && agent_start_projection::arguments_match(&value.arguments)
        )
    });
    let results = replay_positions(events, |event| {
        matches!(
            event,
            Event::ToolResultDisplay(value)
                if value.call_id == call_id
                    && value.tool_name.as_str() == "agent_start"
                    && value.kind == tau_proto::ToolResultKind::Final
                    && value.display.as_ref().is_some_and(|display| {
                        display.info_chips.iter().any(
                            |chip| chip == &format!("@{}", identities.worker),
                        )
                    })
        )
    });
    let calls = replay_positions(events, |event| {
        matches!(
            event,
            Event::ProviderResponseFinished(finished)
                if finished.agent_id == identities.main
                    && matches!(finished.output_items.as_slice(), [ContextItem::ToolCall(call)]
                        if call.call_id == call_id
                            && call.name.as_str() == "agent_start"
                            && agent_start_projection::arguments_match(&call.arguments)
                    )
        )
    });
    let ([request], [start], [result], [call]) = (
        requests.as_slice(),
        starts.as_slice(),
        results.as_slice(),
        calls.as_slice(),
    ) else {
        return Err(format!(
            "S8 replayed agent_start counts changed: request={requests:?}, start={starts:?}, \
             result={results:?}, call={calls:?}"
        )
        .into());
    };
    if !(request < start && start < call && call < result) {
        return Err("S8 replayed agent_start lifecycle ordering changed".into());
    }
    let lifecycle = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::ToolRequest(value) => Some((
                "request",
                value.call_id.as_str(),
                observed.replay,
                observed.recorded_at.is_some(),
            )),
            Event::ToolStarted(value) => Some((
                "started",
                value.call_id.as_str(),
                observed.replay,
                observed.recorded_at.is_some(),
            )),
            Event::ToolResultDisplay(value) => Some((
                "result",
                value.call_id.as_str(),
                observed.replay,
                observed.recorded_at.is_some(),
            )),
            Event::ToolError(value) => Some((
                "error",
                value.call_id.as_str(),
                observed.replay,
                observed.recorded_at.is_some(),
            )),
            Event::ProviderToolResult(value) => Some((
                "provider_result",
                value.call_id.as_str(),
                observed.replay,
                observed.recorded_at.is_some(),
            )),
            Event::ProviderToolError(value) => Some((
                "provider_error",
                value.call_id.as_str(),
                observed.replay,
                observed.recorded_at.is_some(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    if lifecycle
        != [
            ("request", "s8-agent-start", true, true),
            ("started", "s8-agent-start", true, true),
            ("result", "s8-agent-start", true, true),
        ]
    {
        return Err(format!("S8 replayed agent_start family changed: {lifecycle:?}").into());
    }
    let tool_call_responses = events
        .iter()
        .filter_map(|observed| {
            matches!(
                &observed.event,
                Event::ProviderResponseFinished(finished)
                    if finished.output_items.iter().any(
                        |item| matches!(item, ContextItem::ToolCall(_))
                    )
            )
            .then_some((observed.replay, observed.recorded_at.is_some()))
        })
        .collect::<Vec<_>>();
    if tool_call_responses != [(true, true)] {
        return Err(format!(
            "S8 replayed provider tool-call responses changed: {tool_call_responses:?}"
        )
        .into());
    }
    Ok(())
}

fn replay_positions(events: &[ObservedEvent], predicate: impl Fn(&Event) -> bool) -> Vec<usize> {
    events
        .iter()
        .enumerate()
        .filter_map(|(index, observed)| {
            (observed.replay && observed.recorded_at.is_some() && predicate(&observed.event))
                .then_some(index)
        })
        .collect()
}

fn events_after_agent_boundary(
    events: &[ObservedEvent],
    boundary: usize,
    agent_id: &AgentId,
) -> bool {
    let prompt_owners = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::AgentPromptCreated(value) => {
                Some((value.agent_prompt_id.clone(), value.agent_id.clone()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let call_owners = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::ToolRequest(value) => Some((value.call_id.clone(), value.agent_id.clone())),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    events.iter().skip(boundary + 1).any(|observed| {
        observed.replay && event_owned_by(&observed.event, agent_id, &prompt_owners, &call_owners)
    })
}

fn event_owned_by(
    event: &Event,
    agent_id: &AgentId,
    prompt_owners: &BTreeMap<tau_proto::AgentPromptId, AgentId>,
    call_owners: &BTreeMap<ToolCallId, AgentId>,
) -> bool {
    match event {
        Event::AgentStarted(value) => &value.agent_id == agent_id,
        Event::AgentPromptSubmitted(value) => &value.agent_id == agent_id,
        Event::AgentPromptCreated(value) => &value.agent_id == agent_id,
        Event::AgentPromptStarted(value) => &value.agent_id == agent_id,
        Event::AgentPromptTerminated(value) => &value.agent_id == agent_id,
        Event::AgentInferenceDispatchStarted(value) => &value.agent_id == agent_id,
        Event::AgentStatsUpdated(value) => &value.agent_id == agent_id,
        Event::AgentWatchesUpdated(value) => &value.watcher_id == agent_id,
        Event::ProviderPromptSubmitted(value) => {
            prompt_owners.get(&value.agent_prompt_id) == Some(agent_id)
        }
        Event::ProviderResponseFinished(value) => &value.agent_id == agent_id,
        Event::ProviderToolResult(value) => call_owners.get(&value.call_id) == Some(agent_id),
        Event::ProviderToolError(value) => call_owners.get(&value.call_id) == Some(agent_id),
        Event::ToolRequest(value) => &value.agent_id == agent_id,
        Event::ToolStarted(value) => &value.agent_id == agent_id,
        Event::ToolResultDisplay(value) => call_owners.get(&value.call_id) == Some(agent_id),
        Event::ToolError(value) => call_owners.get(&value.call_id) == Some(agent_id),
        Event::AgentMessageReceived(value) => &value.recipient_id == agent_id,
        _ => false,
    }
}

/// Validates the exact two-row live durable main/worker roster.
pub(super) fn assert_roster(
    roster: &[SessionAgentListEntry],
    identities: &Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    if roster.len() != 2 {
        return Err(format!("S8 restored roster has {} rows", roster.len()).into());
    }
    for (agent_id, role, parent, display_name, navigation_mode) in [
        (
            &identities.main,
            "deterministic-main",
            None,
            None,
            AgentNavigationMode::Active,
        ),
        (
            &identities.worker,
            "deterministic-worker",
            Some(&identities.main),
            Some("deterministic worker"),
            AgentNavigationMode::ActiveAuto,
        ),
    ] {
        let row = roster
            .iter()
            .find(|row| &row.agent_id == agent_id)
            .ok_or_else(|| format!("S8 roster omitted {agent_id}"))?;
        if row.persistence != SessionAgentPersistence::Durable
            || row.lifecycle
                != (SessionAgentLifecycle::Live {
                    runtime_state: AgentRuntimeState::Idle,
                    navigation_mode,
                })
        {
            return Err(format!("S8 roster lifecycle changed for {agent_id}: {row:?}").into());
        }
        match &row.facts {
            SessionAgentFacts::Available {
                role: actual_role,
                parent_agent,
                display_name: actual_name,
                ..
            } if actual_role == role
                && parent_agent.as_ref() == parent
                && actual_name.as_deref() == display_name => {}
            facts => {
                return Err(format!("S8 roster facts changed for {agent_id}: {facts:?}").into());
            }
        }
    }
    Ok(())
}

/// Checks the sole live Boot B prompt/response remained worker-owned.
pub(super) fn assert_boot_b_live_work(
    events: &[ObservedEvent],
    identities: &Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let prompts = events
        .iter()
        .filter(|observed| {
            !observed.replay
                && matches!(
                    &observed.event,
                    Event::AgentPromptSubmitted(prompt)
                        if prompt.agent_id == identities.worker
                            && prompt.text == "fresh worker work"
                )
        })
        .count();
    let responses = events
        .iter()
        .filter(|observed| {
            !observed.replay
                && matches!(
                    &observed.event,
                    Event::ProviderResponseFinished(finished)
                        if finished.agent_id == identities.worker
                            && provider_finished_contains(&observed.event, "fresh worker complete")
                )
        })
        .count();
    if prompts != 1 || responses != 1 {
        return Err(format!(
            "S8 fresh worker live counts changed: prompt={prompts}, response={responses}"
        )
        .into());
    }
    if events.iter().any(|observed| {
        !observed.replay
            && match &observed.event {
                Event::AgentPromptSubmitted(prompt) => prompt.agent_id == identities.main,
                Event::AgentPromptCreated(prompt) => prompt.agent_id == identities.main,
                Event::ProviderResponseFinished(finished) => finished.agent_id == identities.main,
                _ => false,
            }
    }) {
        return Err("S8 targeted worker turn executed on the main".into());
    }
    Ok(())
}

/// Checks the exact per-agent provider-turn budget for one boot.
pub(super) fn assert_provider_turns(
    events: &[ObservedEvent],
    identities: &Identities,
    expected: ProviderTurns,
) -> Result<(), Box<dyn std::error::Error>> {
    let prompt_owners = events
        .iter()
        .filter_map(|observed| {
            (!observed.replay)
                .then_some(&observed.event)
                .and_then(|event| match event {
                    Event::AgentPromptCreated(prompt) => {
                        Some((prompt.agent_prompt_id.clone(), prompt.agent_id.clone()))
                    }
                    _ => None,
                })
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let count = |agent_id: &AgentId| {
        events
            .iter()
            .filter(|observed| {
                !observed.replay
                    && matches!(
                        &observed.event,
                        Event::ProviderPromptSubmitted(submitted)
                            if prompt_owners.get(&submitted.agent_prompt_id) == Some(agent_id)
                    )
            })
            .count()
    };
    let actual = ProviderTurns {
        main: count(&identities.main),
        worker: count(&identities.worker),
    };
    if actual != expected {
        return Err(format!(
            "S8 provider-turn budget changed: main={}, worker={} (expected {}, {})",
            actual.main, actual.worker, expected.main, expected.worker
        )
        .into());
    }
    Ok(())
}

/// Checks that the fake provider is the sole ready extension.
pub(super) fn assert_exact_ready(
    events: &[ObservedEvent],
) -> Result<(), Box<dyn std::error::Error>> {
    let ready = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::ExtensionReady(ready) => Some(ready.extension_name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if ready != ["e2e-fake-provider"] {
        return Err(format!("S8 extension Ready set changed: {ready:?}").into());
    }
    Ok(())
}
