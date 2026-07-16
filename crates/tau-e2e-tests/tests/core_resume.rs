#![cfg(unix)]

use std::path::Path;
use std::time::{Duration, Instant};

use tau_e2e_tests::{DurableSnapshot, ScenarioActionV2, ScenarioLaneV2, ScenarioV2};
use tau_proto::{
    AgentId, AgentRuntimeState, CborValue, ContextItem, Event, SessionId, SessionStartReason,
    ToolCallId,
};

#[path = "core_resume/gate_fixture.rs"]
mod gate_fixture;
#[path = "core_resume/observer.rs"]
mod observer;
#[path = "core_resume/pty_process.rs"]
mod pty_process;

use gate_fixture::GateFixture;
use observer::{ObservedEvent, SideObserver, discover_daemon};
use pty_process::PtyProcess;

const FAKE_PROVIDER: &str = env!("CARGO_BIN_EXE_tau-e2e-fake-provider");
const DEADLINE: Duration = Duration::from_secs(20);

/// Proves a completed real dummy-tool call never repaints as pending in a new
/// spawned Tau UI, while the same durable agent remains useful after cold
/// resume.
#[test]
fn spawned_tau_resume_keeps_completed_dummy_tool_terminal_and_continues()
-> Result<(), Box<dyn std::error::Error>> {
    let nonce = format!("{:x}", std::process::id());
    let before = format!("opud-before-{nonce}");
    let after = format!("opud-after-{nonce}");
    let call_id = ToolCallId::from(format!("opud-call-{nonce}"));
    let tool_complete = format!("opud-tool-complete-{nonce}");
    let after_complete = format!("opud-after-complete-{nonce}");
    let scenario = ScenarioV2::new(
        "spawned-tau-cold-resume",
        vec![ScenarioLaneV2 {
            ctx_id: before.clone(),
            actions: vec![
                ScenarioActionV2::DummyToolCall {
                    user_text: before.clone(),
                    call_id: call_id.clone(),
                },
                ScenarioActionV2::DummyToolResult {
                    user_text: before.clone(),
                    call_id: call_id.clone(),
                    response: tool_complete.clone(),
                },
                ScenarioActionV2::Text {
                    user_text: after.clone(),
                    response: after_complete.clone(),
                },
            ],
        }],
    );
    let fixture = GateFixture::new(&scenario, Path::new(FAKE_PROVIDER))?;

    let mut boot_a = PtyProcess::spawn(
        fixture.command(None),
        false,
        Some((
            fixture.artifact_path("boot-a-pty.raw.bounded"),
            fixture.artifact_path("boot-a-pty.normalized.txt"),
        )),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let (socket_a, session_id) = discover_daemon(fixture.runtime_home(), None, deadline)?;
    let mut observer_a = SideObserver::connect(
        &socket_a,
        &session_id,
        fixture.artifact_path("boot-a-observer.json"),
        deadline,
    )?;
    wait_extensions(&mut observer_a, deadline)?;
    boot_a.send_line(&before)?;
    let agent_id = wait_for_agent(&mut observer_a, &session_id, deadline)?;
    wait_for_terminal_turn(
        &mut observer_a,
        &agent_id,
        &call_id,
        &tool_complete,
        deadline,
    )?;
    assert_tool_admission(&observer_a.events, &agent_id, &call_id, false)?;
    boot_a.wait_for(&tool_complete, deadline)?;
    let frame_a = boot_a.wait_ready(deadline)?;
    assert_terminal_tool_row(&frame_a)?;
    assert_exact_ready_set(&observer_a.events)?;
    fixture.write_artifact("boot-a-pty.raw.bounded", &boot_a.raw()?)?;
    fixture.write_artifact("boot-a-pty.normalized.txt", frame_a.as_bytes())?;
    fixture.write_artifact(
        "boot-a-observer.json",
        &serde_json::to_vec_pretty(&observer_a.events)?,
    )?;
    drop(observer_a);
    boot_a.finish()?;
    fixture.require_boot_gone(session_id.as_str())?;

    let snapshot_a = DurableSnapshot::load(&fixture.tau_state(), &session_id)?;
    assert_eq!(snapshot_a.agent_id, agent_id);
    assert_durable_tool(&snapshot_a, &before, &call_id, &tool_complete)?;

    let mut boot_b = PtyProcess::spawn(
        fixture.command(Some(session_id.as_str())),
        true,
        Some((
            fixture.artifact_path("boot-b-pty.raw.bounded"),
            fixture.artifact_path("boot-b-pty.normalized.txt"),
        )),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let (socket_b, discovered_b) =
        discover_daemon(fixture.runtime_home(), Some(&session_id), deadline)?;
    assert_eq!(discovered_b, session_id);
    let mut observer_b = SideObserver::connect(
        &socket_b,
        &session_id,
        fixture.artifact_path("boot-b-observer.json"),
        deadline,
    )?;
    wait_for_resume_boundary(
        &mut observer_b,
        &session_id,
        &agent_id,
        &before,
        &call_id,
        &tool_complete,
        deadline,
    )?;
    wait_extensions(&mut observer_b, deadline)?;
    boot_b.wait_for(&tool_complete, deadline)?;
    let restored = boot_b.wait_ready(deadline)?;
    assert_terminal_tool_row(&restored)?;
    boot_b.require_no_tool_violation()?;

    boot_b.send_line(&after)?;
    wait_for_fresh_turn(&mut observer_b, &agent_id, &after_complete, deadline)?;
    boot_b.wait_for(&after_complete, deadline)?;
    let final_frame = boot_b.wait_ready(deadline)?;
    assert_terminal_tool_row(&final_frame)?;
    boot_b.finish_tool_monitoring()?;
    let old_position = final_frame
        .find(&tool_complete)
        .ok_or("restored marker disappeared after fresh turn")?;
    let new_position = final_frame
        .find(&after_complete)
        .ok_or("fresh completion marker missing")?;
    assert!(old_position < new_position);
    observer_b.drain_available()?;
    assert_no_live_old_execution(&observer_b.events, &before, &call_id, &tool_complete)?;
    assert_exact_ready_set(&observer_b.events)?;
    fixture.write_artifact("boot-b-pty.raw.bounded", &boot_b.raw()?)?;
    fixture.write_artifact("boot-b-pty.normalized.txt", final_frame.as_bytes())?;
    fixture.write_artifact(
        "boot-b-observer.json",
        &serde_json::to_vec_pretty(&observer_b.events)?,
    )?;
    drop(observer_b);
    boot_b.finish()?;
    fixture.require_boot_gone(session_id.as_str())?;

    let snapshot_b = DurableSnapshot::load(&fixture.tau_state(), &session_id)?;
    snapshot_b.require_prefix(&snapshot_a)?;
    assert_eq!(snapshot_b.agent_id, agent_id);
    assert_durable_tool(&snapshot_b, &before, &call_id, &tool_complete)?;
    assert_eq!(count_text(&snapshot_b, &after), 1);
    assert_eq!(count_text(&snapshot_b, &after_complete), 1);
    fixture.complete();
    Ok(())
}

fn wait_extensions(
    observer: &mut SideObserver,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut fake = observer.events.iter().any(|observed| {
        matches!(
            &observed.event,
            Event::ExtensionReady(ready) if ready.extension_name.as_str() == "e2e-fake-provider"
        )
    });
    let mut dummy = observer.events.iter().any(|observed| {
        matches!(
            &observed.event,
            Event::ExtensionReady(ready) if ready.extension_name.as_str() == "test-dummy"
        )
    });
    while !(fake && dummy) {
        let observed = observer.recv_until(deadline, |observed| {
            matches!(&observed.event, Event::ExtensionReady(_))
        })?;
        if let Event::ExtensionReady(ready) = observed.event {
            fake |= ready.extension_name.as_str() == "e2e-fake-provider";
            dummy |= ready.extension_name.as_str() == "test-dummy";
        }
    }
    Ok(())
}

fn wait_for_agent(
    observer: &mut SideObserver,
    session_id: &SessionId,
    deadline: Instant,
) -> Result<AgentId, Box<dyn std::error::Error>> {
    let observed = observer.recv_until(deadline, |observed| {
        matches!(
            &observed.event,
            Event::SessionAgentLoaded(loaded) if &loaded.session_id == session_id
        )
    })?;
    let Event::SessionAgentLoaded(loaded) = observed.event else {
        unreachable!()
    };
    Ok(loaded.agent_id)
}

fn wait_for_terminal_turn(
    observer: &mut SideObserver,
    agent_id: &AgentId,
    call_id: &ToolCallId,
    marker: &str,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    observer.recv_until(deadline, |observed| {
        matches!(
            &observed.event,
            Event::ToolResult(result)
                if &result.call_id == call_id
                    && result.result == CborValue::Text("restart succeeded".to_owned())
        )
    })?;
    observer.recv_until(deadline, |observed| {
        matches!(
            &observed.event,
            Event::ProviderResponseFinished(finished)
                if &finished.agent_id == agent_id
                    && provider_finished_contains(&observed.event, marker)
        )
    })?;
    observer.recv_until(deadline, |observed| {
        matches!(
            &observed.event,
            Event::AgentStatsUpdated(stats)
                if &stats.agent_id == agent_id
                    && stats.runtime_state == AgentRuntimeState::Idle
                    && stats.tools.in_flight == 0
        )
    })?;
    Ok(())
}

fn wait_for_resume_boundary(
    observer: &mut SideObserver,
    session_id: &SessionId,
    agent_id: &AgentId,
    old_prompt: &str,
    call_id: &ToolCallId,
    old_marker: &str,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    observer.recv_until(deadline, |observed| {
        matches!(
            &observed.event,
            Event::SessionReplayComplete(done)
                if &done.session_id == session_id && done.error.is_none()
        )
    })?;
    let started = observer.events.iter().position(|observed| {
        matches!(
            &observed.event,
            Event::SessionStarted(started)
                if &started.session_id == session_id
                    && started.reason == SessionStartReason::Resume
        )
    });
    let loaded = observer.events.iter().position(|observed| {
        matches!(
            &observed.event,
            Event::SessionAgentLoaded(value)
                if &value.session_id == session_id && &value.agent_id == agent_id
        )
    });
    let request = observer.events.iter().position(|observed| {
        observed.replay
            && observed.recorded_at.is_some()
            && matches!(
                &observed.event,
                Event::ToolRequest(value)
                    if tool_request_matches(value, agent_id, call_id)
            )
    });
    let tool_started = observer.events.iter().position(|observed| {
        observed.replay
            && observed.recorded_at.is_some()
            && matches!(
                &observed.event,
                Event::ToolStarted(value)
                    if tool_started_matches(value, agent_id, call_id)
            )
    });
    let prompt = observer.events.iter().position(|observed| {
        observed.replay
            && observed.recorded_at.is_some()
            && matches!(
                &observed.event,
                Event::AgentPromptSubmitted(value)
                    if &value.agent_id == agent_id && value.text == old_prompt
            )
    });
    let call = observer.events.iter().position(|observed| {
        observed.replay
            && observed.recorded_at.is_some()
            && matches!(
                &observed.event,
                Event::ProviderResponseFinished(finished)
                    if finished.output_items.iter().any(|item| {
                        matches!(item, ContextItem::ToolCall(call) if &call.call_id == call_id)
                    })
            )
    });
    let result = observer.events.iter().position(|observed| {
        observed.replay
            && observed.recorded_at.is_some()
            && matches!(
                &observed.event,
                Event::ToolResult(value)
                    if &value.call_id == call_id
                        && value.tool_name.as_str() == "restart_test_dummy"
                        && value.tool_type == tau_proto::ToolType::Function
                        && value.kind == tau_proto::ToolResultKind::Final
                        && value.result == CborValue::Text("restart succeeded".to_owned())
            )
    });
    let marker = observer.events.iter().position(|observed| {
        observed.replay
            && observed.recorded_at.is_some()
            && matches!(
                &observed.event,
                Event::ProviderResponseFinished(finished)
                    if &finished.agent_id == agent_id
                        && provider_finished_contains(&observed.event, old_marker)
            )
    });
    let agent_done = observer.events.iter().position(|observed| {
        matches!(
            &observed.event,
            Event::AgentReplayComplete(value)
                if &value.agent_id == agent_id && value.error.is_none()
        )
    });
    let session_done = observer.events.iter().position(|observed| {
        matches!(
            &observed.event,
            Event::SessionReplayComplete(value)
                if &value.session_id == session_id && value.error.is_none()
        )
    });
    let (
        Some(started),
        Some(request),
        Some(tool_started),
        Some(loaded),
        Some(prompt),
        Some(call),
        Some(result),
        Some(marker),
        Some(agent_done),
        Some(session_done),
    ) = (
        started,
        request,
        tool_started,
        loaded,
        prompt,
        call,
        result,
        marker,
        agent_done,
        session_done,
    )
    else {
        return Err("resume observer missed identity, tool, or replay boundary".into());
    };
    assert!(started < request && request < tool_started && tool_started < loaded);
    assert!(loaded < prompt && prompt < call);
    assert!(call < result && result < marker);
    assert!(marker < agent_done && agent_done < session_done);
    let prompt_count = observer
        .events
        .iter()
        .filter(|observed| {
            observed.replay
                && matches!(
                    &observed.event,
                    Event::AgentPromptSubmitted(value)
                        if &value.agent_id == agent_id && value.text == old_prompt
                )
        })
        .count();
    let replayed_start_count = observer
        .events
        .iter()
        .filter(|observed| {
            observed.replay
                && observed.recorded_at.is_some()
                && matches!(
                    &observed.event,
                    Event::ToolStarted(value) if &value.call_id == call_id
                )
        })
        .count();
    let call_count = observer
        .events
        .iter()
        .filter(|observed| {
            observed.replay
                && matches!(
                    &observed.event,
                    Event::ProviderResponseFinished(finished)
                        if finished.output_items.iter().any(|item| {
                            matches!(item, ContextItem::ToolCall(call) if &call.call_id == call_id)
                        })
                )
        })
        .count();
    let result_count = observer
        .events
        .iter()
        .filter(|observed| {
            observed.replay
                && matches!(
                    &observed.event,
                    Event::ToolResult(value) if &value.call_id == call_id
                )
        })
        .count();
    let marker_count = observer
        .events
        .iter()
        .filter(|observed| {
            observed.replay && provider_finished_contains(&observed.event, old_marker)
        })
        .count();
    let error_count = observer
        .events
        .iter()
        .filter(|observed| {
            matches!(&observed.event, Event::ToolError(value) | Event::ProviderToolError(value)
                if &value.call_id == call_id)
        })
        .count();
    if (
        prompt_count,
        replayed_start_count,
        call_count,
        result_count,
        marker_count,
        error_count,
    ) != (1, 1, 1, 1, 1, 0)
    {
        return Err(format!(
            "unexpected replay counts: prompt={prompt_count}, start={replayed_start_count}, call={call_count}, \
             result={result_count}, marker={marker_count}, error={error_count}"
        )
        .into());
    }
    assert_tool_admission(&observer.events, agent_id, call_id, true)?;
    Ok(())
}

fn wait_for_fresh_turn(
    observer: &mut SideObserver,
    agent_id: &AgentId,
    marker: &str,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    observer.recv_until(deadline, |observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::ProviderResponseFinished(finished)
                    if &finished.agent_id == agent_id
                        && provider_finished_contains(&observed.event, marker)
            )
    })?;
    observer.recv_until(deadline, |observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::AgentStatsUpdated(stats)
                    if &stats.agent_id == agent_id
                        && stats.runtime_state == AgentRuntimeState::Idle
                        && stats.tools.in_flight == 0
            )
    })?;
    Ok(())
}

fn assert_terminal_tool_row(frame: &str) -> Result<(), Box<dyn std::error::Error>> {
    let row = unique_tool_row(frame)?;
    if !row.contains("ok") || row.contains("pending") || row.contains('…') {
        return Err(format!("tool row is not terminal: {row}").into());
    }
    Ok(())
}

fn unique_tool_row(frame: &str) -> Result<&str, Box<dyn std::error::Error>> {
    let rows = frame
        .lines()
        .filter(|line| line.contains("restart_test_dummy"))
        .collect::<Vec<_>>();
    if rows.len() != 1 {
        return Err(format!(
            "expected one dummy tool row, found {}:\n{frame}",
            rows.len()
        )
        .into());
    }
    Ok(rows[0])
}

fn provider_finished_contains(event: &Event, marker: &str) -> bool {
    matches!(
        event,
        Event::ProviderResponseFinished(finished)
            if finished.output_items.iter().any(|item| {
                matches!(
                    item,
                    ContextItem::Message(message)
                        if message.content.iter().any(|part| {
                            matches!(part, tau_proto::ContentPart::Text { text } if text == marker)
                        })
                )
            })
    )
}

fn assert_no_live_old_execution(
    events: &[ObservedEvent],
    old_prompt: &str,
    call_id: &ToolCallId,
    old_marker: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if events.iter().any(|observed| {
        !observed.replay
            && match &observed.event {
                Event::ToolRequest(value) => &value.call_id == call_id,
                Event::ToolStarted(value) => &value.call_id == call_id,
                Event::ToolResult(value) => &value.call_id == call_id,
                Event::ToolError(value) => &value.call_id == call_id,
                Event::ProviderToolError(value) => &value.call_id == call_id,
                Event::AgentPromptSubmitted(value) => value.text == old_prompt,
                Event::ProviderResponseFinished(finished) => finished.output_items.iter().any(
                    |item| matches!(item, ContextItem::ToolCall(call) if &call.call_id == call_id),
                ) || provider_finished_contains(
                    &observed.event,
                    old_marker,
                ),
                _ => false,
            }
    }) {
        return Err("old prompt/tool lifecycle executed live during Boot B".into());
    }
    Ok(())
}

fn assert_tool_admission(
    events: &[ObservedEvent],
    agent_id: &AgentId,
    call_id: &ToolCallId,
    replay: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let requests = events
        .iter()
        .enumerate()
        .filter_map(|(index, observed)| {
            (observed.replay == replay
                && (!replay || observed.recorded_at.is_some())
                && matches!(
                    &observed.event,
                    Event::ToolRequest(value)
                        if tool_request_matches(value, agent_id, call_id)
                ))
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let starts = events
        .iter()
        .enumerate()
        .filter_map(|(index, observed)| {
            (observed.replay == replay
                && (!replay || observed.recorded_at.is_some())
                && matches!(
                    &observed.event,
                    Event::ToolStarted(value)
                        if tool_started_matches(value, agent_id, call_id)
                ))
            .then_some(index)
        })
        .collect::<Vec<_>>();
    if requests.len() != 1 || starts.len() != 1 || requests[0] >= starts[0] {
        return Err(format!(
            "tool admission mismatch: replay={replay}, requests={requests:?}, starts={starts:?}"
        )
        .into());
    }
    Ok(())
}

fn tool_request_matches(
    request: &tau_proto::ToolRequest,
    agent_id: &AgentId,
    call_id: &ToolCallId,
) -> bool {
    &request.call_id == call_id
        && request.tool_name.as_str() == "restart_test_dummy"
        && request.tool_type == tau_proto::ToolType::Function
        && request.arguments == CborValue::Map(Vec::new())
        && &request.agent_id == agent_id
        && request.originator.is_user()
}

fn tool_started_matches(
    started: &tau_proto::ToolStarted,
    agent_id: &AgentId,
    call_id: &ToolCallId,
) -> bool {
    &started.call_id == call_id
        && started.tool_name.as_str() == "restart_test_dummy"
        && started.arguments == CborValue::Map(Vec::new())
        && &started.agent_id == agent_id
        && started.originator.is_user()
}

fn assert_exact_ready_set(events: &[ObservedEvent]) -> Result<(), Box<dyn std::error::Error>> {
    let ready = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::ExtensionReady(value) => Some(value.extension_name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if ready.len() != 2 || !ready.contains(&"e2e-fake-provider") || !ready.contains(&"test-dummy") {
        return Err(format!("unexpected exact Ready extension set: {ready:?}").into());
    }
    Ok(())
}

fn assert_durable_tool(
    snapshot: &DurableSnapshot,
    prompt: &str,
    call_id: &ToolCallId,
    marker: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let prompt_positions = snapshot
        .agent_events
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            matches!(
                &record.event,
                Event::AgentPromptSubmitted(value)
                    if value.agent_id == snapshot.agent_id && value.text == prompt
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let call_positions = snapshot
        .agent_events
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            matches!(
                &record.event,
                Event::ProviderResponseFinished(finished)
                    if finished.output_items.iter().any(|item| {
                        matches!(
                            item,
                            ContextItem::ToolCall(call)
                                if &call.call_id == call_id
                                    && call.name.as_str() == "restart_test_dummy"
                                    && call.tool_type == tau_proto::ToolType::Function
                                    && call.arguments == CborValue::Map(Vec::new())
                        )
                    })
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let result_positions = snapshot
        .agent_events
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            matches!(
                &record.event,
                Event::ProviderToolResult(result)
                    if &result.call_id == call_id
                        && result.tool_name.as_str() == "restart_test_dummy"
                        && result.tool_type == tau_proto::ToolType::Function
                        && result.kind == tau_proto::ToolResultKind::Final
                        && result.result == CborValue::Text("restart succeeded".to_owned())
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let marker_positions = snapshot
        .agent_events
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            provider_finished_contains(&record.event, marker).then_some(index)
        })
        .collect::<Vec<_>>();
    if prompt_positions.len() != 1
        || call_positions.len() != 1
        || result_positions.len() != 1
        || marker_positions.len() != 1
        || !(prompt_positions[0] < call_positions[0]
            && call_positions[0] < result_positions[0]
            && result_positions[0] < marker_positions[0])
    {
        return Err(format!(
            "durable tool sequence mismatch: prompt={prompt_positions:?}, \
             call={call_positions:?}, result={result_positions:?}, marker={marker_positions:?}"
        )
        .into());
    }
    Ok(())
}

fn count_text(snapshot: &DurableSnapshot, text: &str) -> usize {
    snapshot
        .agent_events
        .iter()
        .filter(|record| match &record.event {
            Event::AgentPromptSubmitted(prompt) => prompt.text == text,
            Event::ProviderResponseFinished(_) => provider_finished_contains(&record.event, text),
            _ => false,
        })
        .count()
}
