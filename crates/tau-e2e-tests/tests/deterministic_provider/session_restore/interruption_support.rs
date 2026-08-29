//! Shared fail-closed oracles for interrupted inference and foreground tools.

use std::time::{Duration, Instant};

use tau_proto::{
    AgentId, AgentInferenceDispatchStarted, AgentPromptId, Event, NoticeLevel, SessionId,
    ToolCallId,
};

use super::{DeterministicFixture, DurableSessionSnapshot, Observed, SessionRestoreObserver};

/// Waits for the exact live durable-dispatch publication owned by one worker.
pub(super) fn wait_for_worker_dispatch(
    observer: &mut SessionRestoreObserver,
    worker: &AgentId,
) -> Result<AgentInferenceDispatchStarted, Box<dyn std::error::Error>> {
    let mut next = 0;
    loop {
        while let Some(observed) = observer.events.get(next) {
            next += 1;
            if let Event::AgentInferenceDispatchStarted(dispatch) = &observed.event
                && &dispatch.agent_id == worker
            {
                if observed.replay || observed.recorded_at.is_none() {
                    return Err("worker dispatch was not a live durable publication".into());
                }
                return Ok(dispatch.clone());
            }
        }
        observer.recv_one()?;
    }
}

/// Waits until the worker checkpoint can be decoded from its authoritative
/// durable journal while the daemon and held provider prompt are still live.
pub(super) fn wait_for_durable_dispatch(
    fixture: &DeterministicFixture,
    session_id: &SessionId,
    worker: &AgentId,
    expected: &AgentInferenceDispatchStarted,
) -> Result<DurableSessionSnapshot, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_error = None;
    loop {
        match DurableSessionSnapshot::load(fixture.harness_state_dir(), session_id) {
            Ok(snapshot)
                if snapshot.agent_events.get(worker).is_some_and(|events| {
                    events.iter().any(|record| {
                        matches!(
                            &record.event,
                            Event::AgentInferenceDispatchStarted(dispatch)
                                if dispatch == expected
                        )
                    })
                }) =>
            {
                return Ok(snapshot);
            }
            Ok(_) => {}
            Err(error) => last_error = Some(error.to_string()),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "durable worker dispatch did not become readable: {}",
                last_error.as_deref().unwrap_or("checkpoint absent")
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until one exact provider terminal is readable from the authoritative
/// agent journal while the daemon remains live.
///
/// A live `provider.response_finished` delivery can precede completion of its
/// asynchronous durable append. Restart tests must wait for this cut before
/// choosing which other dispatch remains intentionally uncertain.
pub(super) fn wait_for_durable_response(
    fixture: &DeterministicFixture,
    session_id: &SessionId,
    agent_id: &AgentId,
    prompt_id: &AgentPromptId,
) -> Result<DurableSessionSnapshot, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_error = None;
    loop {
        match DurableSessionSnapshot::load(fixture.harness_state_dir(), session_id) {
            Ok(snapshot)
                if snapshot.agent_events.get(agent_id).is_some_and(|events| {
                    events.iter().any(|record| {
                        matches!(
                            &record.event,
                            Event::ProviderResponseFinished(finished)
                                if &finished.agent_prompt_id == prompt_id
                        )
                    })
                }) =>
            {
                return Ok(snapshot);
            }
            Ok(_) => {}
            Err(error) => last_error = Some(error.to_string()),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "durable provider terminal did not become readable: {}",
                last_error.as_deref().unwrap_or("terminal absent")
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits for the fake's exact prompt-correlated live hold readiness record.
pub(super) fn wait_for_hold_readiness(
    fixture: &DeterministicFixture,
    prompt_id: &AgentPromptId,
) -> Result<(), Box<dyn std::error::Error>> {
    let expected = format!("prompt_id={prompt_id} hold_ready");
    let timeout = format!("prompt_id={prompt_id} hold_timeout");
    let canceled = format!("prompt_id={prompt_id} hold_canceled");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let trace = fixture.trace()?;
        if trace.contains(&timeout) || trace.contains(&canceled) {
            return Err(format!(
                "held provider prompt reached an invalid readiness state: {trace}"
            )
            .into());
        }
        let ready = trace.lines().filter(|line| *line == expected).count();
        if ready == 1 {
            return Ok(());
        }
        if 1 < ready {
            return Err(format!(
                "held provider prompt reached an invalid readiness state: {trace}"
            )
            .into());
        }
        if Instant::now() >= deadline {
            return Err(format!("provider hold readiness missing for {prompt_id}").into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Rechecks that the exact ready hold has not terminalized immediately before
/// the process-group kill.
pub(super) fn assert_hold_ready_and_live(
    fixture: &DeterministicFixture,
    prompt_id: &AgentPromptId,
) -> Result<(), Box<dyn std::error::Error>> {
    let trace = fixture.trace()?;
    let ready = format!("prompt_id={prompt_id} hold_ready");
    let timeout = format!("prompt_id={prompt_id} hold_timeout");
    let canceled = format!("prompt_id={prompt_id} hold_canceled");
    if trace.lines().filter(|line| *line == ready).count() != 1
        || trace.contains(&timeout)
        || trace.contains(&canceled)
    {
        return Err(format!("provider hold was not live at the crash cut: {trace}").into());
    }
    Ok(())
}

/// Requires one exact worker dispatch checkpoint and no corresponding durable
/// provider terminal.
pub(super) fn assert_unfinished_worker_dispatch(
    snapshot: &DurableSessionSnapshot,
    worker: &AgentId,
    expected: &AgentInferenceDispatchStarted,
) -> Result<(), Box<dyn std::error::Error>> {
    let records = snapshot
        .agent_events
        .get(worker)
        .ok_or_else(|| format!("durable worker journal missing for {worker}"))?;
    let dispatches = records
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentInferenceDispatchStarted(dispatch) if dispatch == expected
            )
        })
        .count();
    let terminals = records
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::ProviderResponseFinished(finished)
                    if finished.agent_prompt_id == expected.agent_prompt_id
            )
        })
        .count();
    if dispatches != 1 || terminals != 0 {
        return Err(format!(
            "worker dispatch durability changed: dispatches={dispatches}, terminals={terminals}"
        )
        .into());
    }
    Ok(())
}

/// Requires the exact mandatory fail-closed restore warning.
pub(super) fn assert_dispatch_uncertain_notice(
    events: &[Observed],
    worker: &AgentId,
) -> Result<tau_proto::UnixMicros, Box<dyn std::error::Error>> {
    let expected =
        format!("inference dispatch for restored agent `{worker}` is uncertain; retry explicitly");
    let notices = events
        .iter()
        .filter(|observed| {
            matches!(
                &observed.event,
                Event::HarnessNotice(notice)
                    if notice.kind == tau_proto::notice_kind::HARNESS_INTERNAL_WARNING
                        && notice.message == expected
                        && notice.level == NoticeLevel::Warning
                        && notice.purpose == tau_proto::NoticePurpose::Alert
            )
        })
        .collect::<Vec<_>>();
    let [notice] = notices.as_slice() else {
        return Err(
            format!("dispatch-uncertain restore warning delivery changed: {notices:?}").into(),
        );
    };
    if !notice.replay {
        return Err("dispatch-uncertain warning bypassed mandatory catch-up replay".into());
    }
    notice
        .recorded_at
        .ok_or_else(|| "dispatch-uncertain warning lacked its publication timestamp".into())
}

/// Waits for one exact canonical live dummy-hold readiness progress event.
pub(super) fn wait_for_tool_readiness(
    observer: &mut SessionRestoreObserver,
    call_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut next = 0;
    loop {
        while let Some(observed) = observer.events.get(next) {
            next += 1;
            if matches!(
                &observed.event,
                Event::ToolProgress(progress)
                    if progress.call_id.as_str() == call_id
                        && progress.tool_name.as_str() == "restart_test_dummy"
                        && progress.message.as_deref() == Some("hold_no_side_effect ready")
            ) {
                return if observed.replay {
                    Err("tool hold readiness was not a live non-semantic fact".into())
                } else {
                    Ok(())
                };
            }
        }
        observer.recv_one()?;
    }
}

/// Waits until the authoritative stores contain both the interrupted worker's
/// durable ownership journal and its exact restore request/start pair.
///
/// The membership and restore journals are distinct production persistence
/// streams. A live read can observe the later restore append after loading an
/// earlier membership prefix, so the oracle must reject that mixed snapshot
/// and retry until both sides of the ownership relationship are present.
pub(super) fn wait_for_interrupted_tool_snapshot(
    fixture: &DeterministicFixture,
    session_id: &SessionId,
    worker: &AgentId,
    call_id: &str,
) -> Result<DurableSessionSnapshot, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(snapshot) = DurableSessionSnapshot::load(fixture.harness_state_dir(), session_id)
            && snapshot.agent_events.get(worker).is_some_and(|events| {
                events.iter().any(|record| {
                    matches!(
                        &record.event,
                        Event::AgentStarted(started) if &started.agent_id == worker
                    )
                })
            })
            && interrupted_restore_records(&snapshot, worker, call_id).len() == 2
        {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err("interrupted tool restore pair did not become durable".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Returns restore records correlated to one worker and foreground call.
fn interrupted_restore_records<'a>(
    snapshot: &'a DurableSessionSnapshot,
    worker: &AgentId,
    call_id: &str,
) -> Vec<&'a tau_core::PersistedSessionEvent> {
    snapshot
        .restore_events
        .iter()
        .filter(|record| match &record.event {
            Event::ToolRequest(request) => {
                &request.agent_id == worker && request.call_id.as_str() == call_id
            }
            Event::ToolStarted(started) => {
                &started.agent_id == worker && started.call_id.as_str() == call_id
            }
            _ => false,
        })
        .collect()
}

/// Requires exactly one durable request followed by one canonical start.
pub(super) fn assert_interrupted_restore_stream(
    snapshot: &DurableSessionSnapshot,
    worker: &AgentId,
    call_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let records = interrupted_restore_records(snapshot, worker, call_id);
    if !matches!(
        records.as_slice(),
        [
            tau_core::PersistedSessionEvent {
                event: Event::ToolRequest(request),
                ..
            },
            tau_core::PersistedSessionEvent {
                event: Event::ToolStarted(started),
                ..
            }
        ] if request.call_id == started.call_id
            && request.tool_name == started.tool_name
            && request.arguments == started.arguments
            && request.tool_name.as_str() == "restart_test_dummy"
    ) {
        return Err("restore stream lacks the exact interrupted request/start pair".into());
    }
    Ok(())
}

/// Requires that the interrupted call has no terminal result at the crash cut.
pub(super) fn assert_no_terminal_tool_event(
    snapshot: &DurableSessionSnapshot,
    worker: &AgentId,
    call_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let events = snapshot.agent_events.get(worker).ok_or_else(|| {
        format!("interrupted call owner `{worker}` lacks a durable agent journal")
    })?;
    if events.iter().any(|record| {
        matches!(
            &record.event,
            Event::ToolResult(result) | Event::ProviderToolResult(result)
                if result.call_id.as_str() == call_id
        ) || matches!(
            &record.event,
            Event::ToolError(error) | Event::ProviderToolError(error)
                if error.call_id.as_str() == call_id
        )
    }) {
        return Err("interrupted call already had a terminal event at the crash cut".into());
    }
    Ok(())
}

/// Requires one live durable/derived repair pair owned by the worker.
pub(super) fn assert_boot_b_repair_pair(
    fixture: &DeterministicFixture,
    events: &[Observed],
    worker: &AgentId,
    call_id: &str,
    diagnostic: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let repair = events
        .iter()
        .enumerate()
        .filter_map(|(index, observed)| match &observed.event {
            Event::ToolError(error) | Event::ProviderToolError(error)
                if error.tool_name.as_str() == "restart_test_dummy" =>
            {
                Some((index, observed, error))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if repair.len() != 1
        || !matches!(repair[0].1.event, Event::ProviderToolError(_))
        || repair[0].2.call_id.as_str() != call_id
        || repair[0].2.message != diagnostic
        || !repair[0].1.replay
        || repair[0].1.recorded_at.is_none()
    {
        return Err(format!("repair pair changed for worker {worker}: {repair:?}").into());
    }
    assert_single_repair_trace(fixture, call_id)?;
    Ok(())
}

/// Requires the complete strict fake trace to contain one ordered live repair
/// pair across every daemon generation.
pub(super) fn assert_single_repair_trace(
    fixture: &DeterministicFixture,
    call_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let trace = fixture.trace()?;
    let tool = format!("call_id={call_id} repair_tool_error");
    let provider = format!("call_id={call_id} repair_provider_tool_error");
    let lines = trace.lines().collect::<Vec<_>>();
    let tool_positions = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (*line == tool).then_some(index))
        .collect::<Vec<_>>();
    let provider_positions = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (*line == provider).then_some(index))
        .collect::<Vec<_>>();
    if tool_positions.len() != 1
        || provider_positions.len() != 1
        || provider_positions[0] >= tool_positions[0]
    {
        return Err(format!("live repair trace changed for call {call_id}: {trace}").into());
    }
    Ok(())
}

/// Requires resume repair not to redispatch the extension-owned tool.
pub(super) fn assert_no_live_tool_restart(
    events: &[Observed],
    worker: &AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    if events.iter().any(|observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::ToolStarted(started)
                    if &started.agent_id == worker
                        && started.tool_name.as_str() == "restart_test_dummy"
            )
    }) {
        return Err("resume emitted a second live dummy tool start".into());
    }
    Ok(())
}

/// Requires a later resume to publish no new live repair for the balanced call.
pub(super) fn assert_no_duplicate_repair(
    events: &[Observed],
    worker: &AgentId,
    call_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if events.iter().any(|observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::ToolError(error) | Event::ProviderToolError(error)
                    if error.call_id.as_str() == call_id
            )
    }) {
        return Err(format!("resume duplicated repair for worker {worker}").into());
    }
    Ok(())
}

/// Reproduces the harness's exact synthetic interrupted-tool diagnostic.
pub(super) fn interrupted_tool_diagnostic(call_id: &ToolCallId) -> String {
    format!(
        "{}: true\n\nTool call `{call_id}` was interrupted due to session restart. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}
