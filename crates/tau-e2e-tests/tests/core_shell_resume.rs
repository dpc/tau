//! Gate 2: real bundled core-shell state reconstruction across process
//! replacement.

use std::collections as path_std_collections;
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
const SESSION: &str = "deterministic-e2e-session";

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
                        | tau_proto::ContentPart::HarnessInternalText { text } => text.as_str(),
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
