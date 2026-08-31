//! Gate 2 core-shell acceptance: cold state reconstruction plus one closed
//! four-sibling production-shell concurrency oracle.

use std::collections as path_std_collections;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tau_e2e_tests::{
    DeterministicFixture, DurableSnapshot, ScenarioActionV2, ScenarioLaneV2, ScenarioV2,
};
use tau_proto::{
    CborValue, Event, HarnessInputMessage, PromptMessageClass, PromptOriginator,
    ProviderStopReason, SessionId, UiCreateAgent,
};

#[path = "deterministic_provider/daemon_support.rs"]
mod daemon_support;
use daemon_support::*;

const FAKE_PROVIDER: &str = env!("CARGO_BIN_EXE_tau-e2e-fake-provider");
const HARNESS_DAEMON: &str = env!("CARGO_BIN_EXE_tau-e2e-harness-daemon");
const SHELL_PROBE: &str = env!("CARGO_BIN_EXE_tau-e2e-shell-probe");
const SESSION: &str = "deterministic-e2e-session";

/// Proves one fake-provider terminal containing four sibling production shell
/// calls reaches the ext-shell worker pool concurrently rather than serially.
#[test]
fn core_shell_four_sibling_commands_overlap() -> Result<(), Box<dyn std::error::Error>> {
    run_core_shell_four_sibling_commands(true)
}

/// Proves a route that advertises one-call guidance still preserves a violating
/// four-call provider terminal and its complete continuation losslessly.
#[test]
fn core_shell_four_sibling_commands_remain_lossless_when_capability_false()
-> Result<(), Box<dyn std::error::Error>> {
    run_core_shell_four_sibling_commands(false)
}

fn run_core_shell_four_sibling_commands(
    advertise_parallel: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    const PROMPT: &str = "run four deterministic sibling shell commands";
    const RESPONSE: &str = "parallel shell commands completed";
    let probe_executable = resolve_executable(SHELL_PROBE)
        .ok_or("the deterministic shell concurrency probe is not executable")?;
    let call_ids = std::array::from_fn(|index| {
        tau_proto::ToolCallId::new(format!("parallel-shell-{}", index + 1))
    });
    let wait_call_ids = std::array::from_fn(|index| {
        tau_proto::ToolCallId::new(format!("parallel-wait-{}", index + 1))
    });
    let scenario = ScenarioV2::new(
        "core-shell-parallel",
        vec![ScenarioLaneV2 {
            ctx_id: "core-shell-parallel-lane".to_owned(),
            actions: vec![
                ScenarioActionV2::CoreShellParallelCalls {
                    user_text: PROMPT.to_owned(),
                    advertise_parallel,
                    call_ids: call_ids.clone(),
                    probe_executable,
                },
                ScenarioActionV2::CoreShellParallelWaits {
                    user_text: PROMPT.to_owned(),
                    advertise_parallel,
                    call_ids: call_ids.clone(),
                    wait_call_ids: wait_call_ids.clone(),
                },
                ScenarioActionV2::CoreShellParallelResult {
                    user_text: PROMPT.to_owned(),
                    advertise_parallel,
                    call_ids: call_ids.clone(),
                    wait_call_ids,
                    response: RESPONSE.to_owned(),
                },
            ],
        }],
    );
    let fixture = DeterministicFixture::new_core_shell_parallel(
        if advertise_parallel {
            "core_shell_four_sibling_commands_overlap"
        } else {
            "core_shell_four_sibling_commands_capability_false"
        },
        &scenario,
        FAKE_PROVIDER,
    )?;
    let socket = fixture.socket_path("parallel-shell");
    let daemon = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
    let mut peer = connect_ui(&socket)?;
    create_agent_without_prompt(&mut peer)?;
    let agent_id = wait_agent_and_context_ready(&mut peer, false, fixture.shell_base())?;
    submit_prompt(&mut peer, &agent_id, "core-shell-parallel-lane", PROMPT)?;
    let final_response = recv_end_turn(&mut peer)?;
    assert_eq!(assistant_text(&final_response.output_items), RESPONSE);
    disconnect_ui(&mut peer)?;
    daemon.finish()?;

    let events = fixture.published_trace_events()?;
    let sibling_terminal = events
        .iter()
        .filter_map(|event| match event {
            Event::ProviderResponseFinished(response) => {
                let calls = response
                    .output_items
                    .iter()
                    .filter_map(|item| match item {
                        tau_proto::ContextItem::ToolCall(call) => Some(call),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                (calls.len() == 4 && calls.iter().all(|call| call.name.as_str() == "shell"))
                    .then_some(calls)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(sibling_terminal.len(), 1);
    assert_eq!(
        sibling_terminal[0]
            .iter()
            .map(|call| &call.call_id)
            .collect::<Vec<_>>(),
        call_ids.iter().collect::<Vec<_>>()
    );

    let first_terminal = events
        .iter()
        .position(|event| match event {
            Event::ProviderToolResult(result) => call_ids.contains(&result.call_id),
            Event::ProviderToolError(error) => call_ids.contains(&error.call_id),
            _ => false,
        })
        .expect("parallel shell calls must terminate");
    let request_positions = call_ids
        .iter()
        .enumerate()
        .map(|(index, call_id)| {
            let expected = sibling_terminal[0][index];
            let positions = events
                .iter()
                .enumerate()
                .filter_map(|(index, event)| {
                    matches!(
                        event,
                        Event::ToolRequest(request)
                            if &request.call_id == call_id
                                && request.tool_name == expected.name
                                && request.tool_type == expected.tool_type
                                && request.arguments == expected.arguments
                                && request.agent_id == agent_id
                    )
                    .then_some(index)
                })
                .collect::<Vec<_>>();
            assert_eq!(positions.len(), 1, "canonical request must be unique");
            positions[0]
        })
        .collect::<Vec<_>>();
    let started_positions = call_ids
        .iter()
        .enumerate()
        .map(|(index, call_id)| {
            let expected = sibling_terminal[0][index];
            let positions = events
                .iter()
                .enumerate()
                .filter_map(|(index, event)| {
                    matches!(
                        event,
                        Event::ToolStarted(started)
                            if &started.call_id == call_id
                                && started.tool_name == expected.name
                                && started.arguments == expected.arguments
                                && started.agent_id == agent_id
                    )
                    .then_some(index)
                })
                .collect::<Vec<_>>();
            assert_eq!(positions.len(), 1, "tool start must be unique");
            positions[0]
        })
        .collect::<Vec<_>>();
    assert!(
        request_positions
            .iter()
            .chain(&started_positions)
            .all(|position| *position < first_terminal),
        "all sibling requests and starts must precede any terminal: requests={request_positions:?}, starts={started_positions:?}, terminal={first_terminal}"
    );
    let background_results = events
        .iter()
        .filter_map(|event| match event {
            Event::ToolBackgroundResult(result) if call_ids.contains(&result.call_id) => {
                Some(&result.call_id)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(background_results.len(), 4);
    assert_eq!(
        background_results
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        call_ids.iter().collect::<std::collections::BTreeSet<_>>(),
        "each shell call must produce exactly one real background result"
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::ToolBackgroundError(error) if call_ids.contains(&error.call_id)
    )));

    let intervals = call_ids
        .iter()
        .map(|call_id| shell_interval(&events, call_id))
        .collect::<Result<Vec<_>, _>>()?;
    let latest_start = intervals
        .iter()
        .map(|interval| interval.start)
        .max()
        .expect("four intervals");
    let earliest_end = intervals
        .iter()
        .map(|interval| interval.end)
        .min()
        .expect("four intervals");
    let first_start = intervals
        .iter()
        .map(|interval| interval.start)
        .min()
        .expect("four intervals");
    let last_end = intervals
        .iter()
        .map(|interval| interval.end)
        .max()
        .expect("four intervals");
    assert!(latest_start < earliest_end, "intervals lack common overlap");
    assert!(
        Duration::from_nanos(last_end - first_start) < Duration::from_secs(6),
        "parallel makespan exceeded six seconds: {intervals:?}"
    );
    for interval in &intervals {
        assert!(
            (Duration::from_millis(2_500)..=Duration::from_secs(5)).contains(&interval.elapsed)
        );
        assert!(interval.start < interval.end);
    }
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves the bundled production core-shell restores a committed per-agent
/// workdir and performs a context-checked relative edit after a cold resume.
#[test]
fn core_shell_workdir_and_relative_edit_survive_cold_resume()
-> Result<(), Box<dyn std::error::Error>> {
    let nonce = format!("gate2-{}", std::process::id());
    let prompt_a = "set the closed project workdir and create the sentinel";
    let prompt_b = "resume and update the closed relative sentinel";
    let scenario = ScenarioV2::new(
        "core-shell-cold-resume",
        vec![ScenarioLaneV2 {
            ctx_id: "core-shell-lane".to_owned(),
            actions: vec![
                ScenarioActionV2::CoreShellWorkdirCall {
                    user_text: prompt_a.to_owned(),
                    call_id: "gate2-workdir".into(),
                },
                ScenarioActionV2::CoreShellWorkdirResult {
                    user_text: prompt_a.to_owned(),
                    call_id: "gate2-workdir".into(),
                    edit_call_id: "gate2-create".into(),
                    nonce: nonce.clone(),
                },
                ScenarioActionV2::CoreShellCreateResult {
                    user_text: prompt_a.to_owned(),
                    call_id: "gate2-create".into(),
                    response: "gate2 boot-a complete".to_owned(),
                },
                ScenarioActionV2::CoreShellResumeEditCall {
                    user_text: prompt_b.to_owned(),
                    call_id: "gate2-resume-edit".into(),
                    nonce: nonce.clone(),
                },
                ScenarioActionV2::CoreShellResumeEditResult {
                    user_text: prompt_b.to_owned(),
                    call_id: "gate2-resume-edit".into(),
                    response: "gate2 boot-b complete".to_owned(),
                },
            ],
        }],
    );
    let fixture = DeterministicFixture::new_core_shell(
        "core_shell_workdir_and_relative_edit_survive_cold_resume",
        &scenario,
        FAKE_PROVIDER,
    )?;
    let project = fixture.shell_base().join("project").canonicalize()?;
    let sentinel = project.join("resume-sentinel.txt");
    let wrong_path = fixture.shell_base().join("resume-sentinel.txt");
    assert!(!sentinel.exists());
    assert!(!wrong_path.exists());
    let canary = format!("outside-canary:{nonce}\n").into_bytes();
    std::fs::write(fixture.outside_canary(), &canary)?;

    let socket_a = fixture.socket_path("core-shell-a");
    let daemon_a = spawn_daemon(&fixture, &socket_a, tau_harness::SessionLaunchStatus::New);
    let mut peer_a = connect_ui(&socket_a)?;
    create_agent_without_prompt(&mut peer_a)?;
    let agent_id = wait_agent_and_context_ready(&mut peer_a, false, &project)?;
    submit_prompt(&mut peer_a, &agent_id, "core-shell-lane", prompt_a)?;
    let final_a = recv_end_turn(&mut peer_a)?;
    assert_eq!(
        assistant_text(&final_a.output_items),
        "gate2 boot-a complete"
    );
    // Headless Tau has no post-EndTurn UI-idle frame. Its quiescent cut is the
    // terminal response plus complete durable facts and process-group teardown;
    // Gate 1 separately owns the UI-idle oracle.
    disconnect_ui(&mut peer_a)?;
    daemon_a.finish()?;

    assert_eq!(
        std::fs::read(&sentinel)?,
        format!("before:{nonce}\n").as_bytes()
    );
    assert!(!wrong_path.exists());
    assert_eq!(std::fs::read(fixture.outside_canary())?, canary);
    let session_id = SessionId::parse(SESSION).expect("known-safe SessionId must be valid");
    let prefix = DurableSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    assert_eq!(
        prefix.metadata_value("ext_core-shell_cwd")?,
        Some(CborValue::Text(project.display().to_string()))
    );
    assert_eq!(
        prefix
            .agent_events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentMetadataSet(set)
                    if set.key.as_str() == "ext_core-shell_cwd"
                        && set.value == CborValue::Text(project.display().to_string())
            ))
            .count(),
        1
    );
    assert_durable_sequence(
        &prefix.agent_events,
        prompt_a,
        &[
            (
                "gate2-workdir",
                "workdir",
                cbor_map(vec![("path", CborValue::Text("project".to_owned()))]),
                CborValue::Text(format!("Workdir changed to {}.", project.display())),
            ),
            (
                "gate2-create",
                "edit",
                edit_args(1, 1, format!("before:{nonce}\n"), ""),
                edit_result(2, format!("before:{nonce}\n").len()),
            ),
        ],
        "gate2 boot-a complete",
    )?;

    let socket_b = fixture.socket_path("core-shell-b");
    let daemon_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut peer_b = connect_ui(&socket_b)?;
    let resumed_agent = wait_agent_and_context_ready(&mut peer_b, true, &project)?;
    assert_eq!(resumed_agent, agent_id);
    submit_prompt(&mut peer_b, &agent_id, "core-shell-b", prompt_b)?;
    let final_b = recv_end_turn(&mut peer_b)?;
    assert_eq!(
        assistant_text(&final_b.output_items),
        "gate2 boot-b complete"
    );
    disconnect_ui(&mut peer_b)?;
    daemon_b.finish()?;

    assert_eq!(
        std::fs::read(&sentinel)?,
        format!("before:{nonce}\nafter:{nonce}\n").as_bytes()
    );
    assert!(!wrong_path.exists());
    assert_eq!(std::fs::read(fixture.outside_canary())?, canary);
    let resumed = DurableSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    resumed.require_prefix(&prefix)?;
    assert_eq!(resumed.agent_id, agent_id);
    assert_eq!(
        resumed.metadata_value("ext_core-shell_cwd")?,
        prefix.metadata_value("ext_core-shell_cwd")?
    );
    let suffix = &resumed.agent_events[prefix.agent_events.len()..];
    assert_durable_sequence(
        suffix,
        prompt_b,
        &[(
            "gate2-resume-edit",
            "edit",
            edit_args(
                1,
                2,
                format!("before:{nonce}\nafter:{nonce}\n"),
                &format!("before:{nonce}"),
            ),
            edit_result(3, format!("before:{nonce}\nafter:{nonce}\n").len()),
        )],
        "gate2 boot-b complete",
    )?;
    assert!(suffix.iter().all(|record| !matches!(
        &record.event,
        Event::AgentMetadataSet(set) if set.key.as_str() == "ext_core-shell_cwd"
    )));
    assert_eq!(
        suffix
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::ProviderToolResult(result) if result.call_id.as_str() == "gate2-resume-edit"
            ))
            .count(),
        1
    );
    fixture.assert_consumed()?;
    Ok(())
}

fn create_agent_without_prompt(
    peer: &mut tau_socket::SocketPeer,
) -> Result<(), Box<dyn std::error::Error>> {
    peer.send(&HarnessInputMessage::emit(Event::UiCreateAgent(
        UiCreateAgent {
            request_id: "core-shell-resume-create".to_owned(),
            literal: false,
            session_id: SESSION
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            role: "deterministic-e2e".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: None,
            message_class: PromptMessageClass::User,
            originator: PromptOriginator::User,
            ctx_id: Some("core-shell-lane".to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )))?;
    Ok(())
}

fn wait_agent_and_context_ready(
    peer: &mut tau_socket::SocketPeer,
    resumed: bool,
    project: &std::path::Path,
) -> Result<tau_proto::AgentId, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut agent = None;
    let mut context_ready_agents = path_std_collections::HashSet::new();
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum ResumeStep {
        Start,
        Metadata,
        Load,
        AgentBoundary,
        Complete,
    }
    let mut step = ResumeStep::Start;
    while Instant::now() < deadline {
        let observed = recv_observed(peer)?;
        match observed.event {
            Event::SessionStarted(started)
                if resumed
                    && started.session_id.as_str() == SESSION
                    && started.reason == tau_proto::SessionStartReason::Resume
                    && step == ResumeStep::Start =>
            {
                step = ResumeStep::Metadata;
            }
            Event::SessionAgentLoaded(loaded)
                if !resumed
                    || (step == ResumeStep::Load && loaded.session_id.as_str() == SESSION) =>
            {
                if resumed {
                    if agent.as_ref() != Some(&loaded.agent_id) {
                        return Err(
                            "resumed session load disagrees with replayed metadata agent".into(),
                        );
                    }
                    step = ResumeStep::AgentBoundary;
                } else {
                    agent = Some(loaded.agent_id);
                }
            }
            Event::ExtensionContextReady(ready) => {
                context_ready_agents.insert(ready.agent_id);
            }
            Event::AgentMetadataSet(set)
                if resumed
                    && step == ResumeStep::Metadata
                    && observed.replay
                    && observed.recorded_at.is_some()
                    && set.key.as_str() == "ext_core-shell_cwd"
                    && set.value == CborValue::Text(project.display().to_string()) =>
            {
                agent = Some(set.agent_id);
                step = ResumeStep::Load;
            }
            Event::AgentReplayComplete(done)
                if resumed
                    && step == ResumeStep::AgentBoundary
                    && !observed.replay
                    && done.error.is_none()
                    && agent.as_ref() == Some(&done.agent_id)
                    && done
                        .session_id
                        .as_ref()
                        .is_some_and(|id| id.as_str() == SESSION) =>
            {
                step = ResumeStep::Complete;
            }
            Event::AgentReplayComplete(_) if resumed => {
                return Err("out-of-order or mismatched resumed agent replay boundary".into());
            }
            Event::SessionReplayComplete(done)
                if resumed
                    && step == ResumeStep::Complete
                    && !observed.replay
                    && done.error.is_none()
                    && done.session_id.as_str() == SESSION =>
            {
                return agent.ok_or_else(|| "resume boundary lacked agent".into());
            }
            Event::SessionReplayComplete(_) if resumed => {
                return Err("out-of-order or mismatched resumed session replay boundary".into());
            }
            Event::AgentMetadataSet(set)
                if !resumed
                    && set.key.as_str() == "ext_core-shell_cwd"
                    && set.value == CborValue::Text(project.display().to_string()) =>
            {
                // Boot A readiness is established by ExtensionContextReady
                // below.
            }
            _ => {}
        }
        if !resumed
            && let Some(agent_id) = &agent
            && context_ready_agents.contains(agent_id)
        {
            return Ok(agent_id.clone());
        }
    }
    Err("core-shell agent/context/replay readiness deadline exceeded".into())
}

fn recv_end_turn(
    peer: &mut tau_socket::SocketPeer,
) -> Result<tau_proto::ProviderResponseFinished, Box<dyn std::error::Error>> {
    loop {
        if let Event::ProviderResponseFinished(finished) = recv_event(peer)?
            && finished.stop_reason == ProviderStopReason::EndTurn
        {
            return Ok(finished);
        }
    }
}

fn assistant_text(items: &[tau_proto::ContextItem]) -> String {
    items
        .iter()
        .find_map(|item| match item {
            tau_proto::ContextItem::Message(message) => Some(
                message
                    .content
                    .iter()
                    .map(|part| match part {
                        tau_proto::ContentPart::Text { text }
                        | tau_proto::ContentPart::SyntheticCompactionSummary { text }
                        | tau_proto::ContentPart::HarnessInternalText { text } => text.as_str(),
                        tau_proto::ContentPart::UrlCitation { .. }
                        | tau_proto::ContentPart::CitationMetadataInvalid => "",
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

fn assert_durable_sequence(
    records: &[tau_core::PersistedAgentEvent],
    prompt: &str,
    calls: &[(&str, &str, CborValue, CborValue)],
    marker: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let positions = records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| match &record.event {
            Event::AgentPromptSubmitted(value) if value.text == prompt => Some((index, "prompt")),
            Event::ProviderResponseFinished(value)
                if value.output_items.iter().any(|item| {
                    matches!(
                        item,
                        tau_proto::ContextItem::Message(message)
                            if message.content.iter().any(|part| matches!(
                                part, tau_proto::ContentPart::Text { text } if text == marker
                            ))
                    )
                }) =>
            {
                Some((index, "marker"))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if positions
        .iter()
        .filter(|(_, kind)| *kind == "prompt")
        .count()
        != 1
        || positions
            .iter()
            .filter(|(_, kind)| *kind == "marker")
            .count()
            != 1
    {
        return Err("durable prompt/final marker is not unique".into());
    }
    let prompt_position = positions
        .iter()
        .find(|(_, kind)| *kind == "prompt")
        .ok_or("missing durable prompt")?
        .0;
    let mut order = vec![prompt_position];
    for (call_id, tool_name, expected_arguments, expected_result) in calls {
        let calls = records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                matches!(
            &record.event,
            Event::ProviderResponseFinished(value) if value.output_items.iter().any(|item| matches!(
                item, tau_proto::ContextItem::ToolCall(call)
                    if call.call_id.as_str() == *call_id && call.name.as_str() == *tool_name
                        && &call.arguments == expected_arguments
            ))
        ).then_some(index)
            })
            .collect::<Vec<_>>();
        let results = records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                matches!(
                    &record.event,
                    Event::ProviderToolResult(result)
                        if result.call_id.as_str() == *call_id
                            && result.tool_name.as_str() == *tool_name
                    && result.kind == tau_proto::ToolResultKind::Final
                    && &result.result == expected_result
                )
                .then_some(index)
            })
            .collect::<Vec<_>>();
        if calls.len() != 1 || results.len() != 1 {
            return Err(format!("durable {call_id} call/result is not unique").into());
        }
        order.extend([calls[0], results[0]]);
        if records.iter().any(|record| {
            matches!(
                &record.event,
                Event::ProviderToolError(error) | Event::ToolError(error)
                    if error.call_id.as_str() == *call_id
            )
        }) {
            return Err(format!("durable {call_id} has a correlated error").into());
        }
    }
    order.push(
        positions
            .iter()
            .find(|(_, kind)| *kind == "marker")
            .ok_or("missing durable marker")?
            .0,
    );
    if !order.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err(format!("durable lifecycle order mismatch: {order:?}").into());
    }
    Ok(())
}

fn cbor_map(fields: Vec<(&str, CborValue)>) -> CborValue {
    CborValue::Map(
        fields
            .into_iter()
            .map(|(key, value)| (CborValue::Text(key.to_owned()), value))
            .collect(),
    )
}

fn edit_args(start: i64, end: i64, text: String, context: &str) -> CborValue {
    cbor_map(vec![
        ("path", CborValue::Text("resume-sentinel.txt".to_owned())),
        (
            "edits",
            CborValue::Array(vec![cbor_map(vec![
                ("start_line", CborValue::Integer(start.into())),
                ("end_line_exclusive", CborValue::Integer(end.into())),
                ("newText", CborValue::Text(text)),
                ("context_line", CborValue::Text(context.to_owned())),
            ])]),
        ),
    ])
}

fn edit_result(new_max_valid_start_line: i64, total_bytes: usize) -> CborValue {
    cbor_map(vec![
        ("edits", CborValue::Integer(1_i64.into())),
        ("changed", CborValue::Bool(true)),
        (
            "new_max_valid_start_line",
            CborValue::Integer(new_max_valid_start_line.into()),
        ),
        (
            "total_bytes",
            CborValue::Integer((total_bytes as i64).into()),
        ),
    ])
}

fn resolve_executable(path: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;

    let candidate = PathBuf::from(path);
    (candidate
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        && Command::new(&candidate)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success()))
    .then(|| candidate.canonicalize().ok())
    .flatten()
}

/// One identity-tagged monotonic execution interval reported by a sibling
/// shell process.
#[derive(Debug)]
struct ShellInterval {
    /// Monotonic process-local start timestamp in nanoseconds.
    start: u64,
    /// Monotonic process-local end timestamp in nanoseconds.
    end: u64,
    /// Reported command duration.
    elapsed: Duration,
}

fn shell_interval(
    events: &[Event],
    call_id: &tau_proto::ToolCallId,
) -> Result<ShellInterval, Box<dyn std::error::Error>> {
    let output = events
        .iter()
        .find_map(|event| match event {
            Event::ToolBackgroundResult(result) if &result.call_id == call_id => {
                cbor_text_field(&result.result, "output")
            }
            _ => None,
        })
        .ok_or_else(|| format!("missing shell output for {call_id}"))?;
    let mut start = None;
    let mut end = None;
    let mut elapsed: Option<f64> = None;
    for field in output
        .strip_prefix("out ")
        .unwrap_or(output)
        .split_whitespace()
    {
        if let Some(value) = field.strip_prefix("start_ns=") {
            start = Some(value.parse()?);
        } else if let Some(value) = field.strip_prefix("end_ns=") {
            end = Some(value.parse()?);
        } else if let Some(value) = field.strip_prefix("elapsed_ms=") {
            elapsed = Some(value.parse()?);
        }
    }
    Ok(ShellInterval {
        start: start.ok_or("missing start_ns")?,
        end: end.ok_or("missing end_ns")?,
        elapsed: Duration::from_secs_f64(elapsed.ok_or("missing elapsed_ms")? / 1_000.0),
    })
}

fn cbor_text_field<'a>(value: &'a CborValue, key: &str) -> Option<&'a str> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(entry_key, entry_value)| {
        matches!(entry_key, CborValue::Text(entry_key) if entry_key == key)
            .then(|| match entry_value {
                CborValue::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .flatten()
    })
}
