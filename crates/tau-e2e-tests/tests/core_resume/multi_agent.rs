//! S8 public-terminal acceptance over a headless-created main/worker session.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use tau_e2e_tests::{
    DurableSessionSnapshot, ScenarioActionV2, ScenarioLaneV2, ScenarioV2, WatchNotificationV2,
};
use tau_proto::{AgentId, SessionAgentListScope, SessionId};

#[path = "multi_agent/agent_start_projection.rs"]
mod agent_start_projection;
#[path = "multi_agent/durable_oracles.rs"]
mod durable_oracles;
#[path = "multi_agent/observer_oracles.rs"]
mod observer_oracles;
#[path = "multi_agent/terminal_oracles.rs"]
mod terminal_oracles;

use durable_oracles::*;
use observer_oracles::*;
use terminal_oracles::*;

use super::gate_fixture::GateFixture;
use super::headless_process::HeadlessProcess;
use super::observer::SideObserver;
use super::pty_process::{PtyArtifacts, PtyProcess};
use super::{DEADLINE, FAKE_PROVIDER, discover_daemon};

const HARNESS_DAEMON: &str = env!("CARGO_BIN_EXE_tau-e2e-harness-daemon");
const SESSION: &str = "deterministic-e2e-session";
const MAIN_PROMPT: &str = "start the deterministic worker";
const WORKER_PROMPT: &str = "Complete the deterministic worker instruction.";
const WORKER_RESPONSE: &str = "worker boot-a complete";
const MAIN_START_RESPONSE: &str = "worker start accepted";
const MAIN_FINAL_RESPONSE: &str = "worker completion observed";
const HIDDEN_MODEL_SENTINEL: &str = tau_e2e_tests::FAKE_MODEL_ID;
const WORKER_INITIAL: &str = concat!(
    "[tau-internal]: You were started by an agent `main`. Your responses will be delivered to it. ",
    "You can use the `message` tool to communicate with agents.\n\n",
    "Complete the deterministic worker instruction."
);
const RESTORE_NOTICE: &str = concat!(
    "[tau-internal] Previous session was interrupted and restored. Less than 1 minute has passed ",
    "since the last recorded session event, and the state of the world might have changed. ",
    "Session-scoped tool and extension state may also have changed; inspect current tool state ",
    "and recreate timers or other session-scoped setup if still needed."
);

/// Proves two attached public UIs materialize the same ID-keyed main/worker
/// transcripts while keeping stable-ID selection entirely connection-local.
#[test]
fn attached_public_terminals_select_agents_independently() -> Result<(), Box<dyn std::error::Error>>
{
    let scenario = scenario();
    let fixture = GateFixture::new_multi_agent(&scenario, Path::new(FAKE_PROVIDER))?;
    let session_id = SessionId::parse(SESSION).expect("known-safe SessionId must be valid");
    let socket = fixture.headless_socket();
    let daemon = HeadlessProcess::spawn(
        fixture.headless_command(Path::new(HARNESS_DAEMON), &socket),
        socket.clone(),
        fixture.artifact_path("selection-daemon.stderr"),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let mut observer = SideObserver::connect(
        &socket,
        &session_id,
        fixture.artifact_path("selection-observer.json"),
        deadline,
    )?;
    observer.wait_for_extension("e2e-fake-provider", deadline)?;
    observer.create_main(&session_id, "s8-main", MAIN_PROMPT)?;
    wait_marker(&mut observer, "worker completion observed", deadline)?;
    wait_two_idle(&mut observer, deadline)?;
    let identities = Identities::from_events(&observer.events)?;
    assert_boot_a(&observer.events, &session_id, &identities)?;
    let roster = observer.roster(&session_id, SessionAgentListScope::Current, deadline)?;
    assert_roster(&roster, &identities)?;
    let durable_before = DurableSessionSnapshot::load(&fixture.tau_state(), &session_id)?;
    assert_snapshot_a(&durable_before, &identities)?;
    let setup_actions = matched_actions(&fixture)?;
    if setup_actions != 4 {
        return Err(format!(
            "selection fixture consumed {setup_actions} setup actions, expected 4"
        )
        .into());
    }
    observer.disconnect()?;
    drop(observer);
    daemon.finish()?;
    fixture.require_boot_gone(session_id.as_str())?;

    let mut first = PtyProcess::spawn(
        fixture.command(Some(session_id.as_str())),
        true,
        Some(PtyArtifacts::new(
            fixture.artifact_path("selection-first.raw.bounded"),
            fixture.artifact_path("selection-first.normalized.txt"),
        )),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let (live_socket, discovered) =
        discover_daemon(fixture.runtime_home(), Some(&session_id), deadline)?;
    if discovered != session_id {
        return Err(format!(
            "selection fixture resumed session `{discovered}`, expected `{session_id}`"
        )
        .into());
    }
    let mut observer = SideObserver::connect(
        &live_socket,
        &session_id,
        fixture.artifact_path("selection-observer.json"),
        deadline,
    )?;
    wait_resume_boundaries(&mut observer, &session_id, &identities, deadline)?;
    observer.wait_for_extension("e2e-fake-provider", deadline)?;
    first.wait_for("worker completion observed", deadline)?;
    let mut second = PtyProcess::spawn(
        fixture.attach_command(session_id.as_str()),
        false,
        Some(PtyArtifacts::new(
            fixture.artifact_path("selection-second.raw.bounded"),
            fixture.artifact_path("selection-second.normalized.txt"),
        )),
    )?;
    second.wait_for("worker completion observed", deadline)?;
    let trace_before_selection = fixture.trace()?;
    let durable_before_selection = DurableSessionSnapshot::load(&fixture.tau_state(), &session_id)?;
    durable_before_selection.require_prefix(&durable_before)?;
    let observer_before_selection = observer.events.len();

    let first_rows = select_all_agents(
        &mut first,
        [&identities.main, &identities.worker],
        &identities,
        deadline,
    )?;
    let second_rows = select_all_agents(
        &mut second,
        [&identities.worker, &identities.main],
        &identities,
        deadline,
    )?;
    if first_rows != second_rows {
        return Err(format!(
            "attached UIs materialized different ID-keyed transcript rows: \
             first={first_rows:?}, second={second_rows:?}"
        )
        .into());
    }

    first.send_line(&format!(":agent switch {}", identities.main))?;
    first.wait_ready_for(identities.main.as_str(), deadline)?;
    second.send_line(&format!(":agent switch {}", identities.main))?;
    second.wait_ready_for(identities.main.as_str(), deadline)?;
    first.send_line(&format!(":agent switch {}", identities.worker))?;
    first.wait_for("This active-auto agent is idle", deadline)?;
    second.send_line(":agent")?;
    let second_still_main = second.wait_for(&format!("current: {}", identities.main), deadline)?;
    assert_transcript_rows(&second_still_main, &identities.main, &identities)?;

    first.send_line(&format!(":agent switch {}", identities.worker))?;
    first.wait_for("This active-auto agent is idle", deadline)?;
    second.send_line(&format!(":agent switch {}", identities.worker))?;
    second.wait_for("This active-auto agent is idle", deadline)?;
    second.send_line(&format!(":agent switch {}", identities.main))?;
    second.wait_ready_for(identities.main.as_str(), deadline)?;
    first.send_line(":agent")?;
    let first_still_worker =
        first.wait_for(&format!("current: {}", identities.worker), deadline)?;
    assert_transcript_rows(&first_still_worker, &identities.worker, &identities)?;

    let post_selection_roster =
        observer.roster(&session_id, SessionAgentListScope::Current, deadline)?;
    assert_roster(&post_selection_roster, &identities)?;
    observer.drain_available()?;
    if let Some(offending) = observer.events[observer_before_selection..]
        .iter()
        .find(|observed| {
            matches!(
                observed.event,
                tau_proto::Event::ProviderPromptSubmitted(_)
                    | tau_proto::Event::AgentStatsUpdated(_)
            )
        })
    {
        return Err(format!(
            "agent selection changed provider or agent runtime facts: {offending:?}"
        )
        .into());
    }

    fixture.write_artifact(
        "selection-observer.json",
        &serde_json::to_vec_pretty(&observer.events)?,
    )?;
    fixture.write_artifact(
        "selection-first.normalized.txt",
        first_still_worker.as_bytes(),
    )?;
    drop(observer);
    second.finish()?;
    first.finish()?;
    fixture.require_boot_gone(session_id.as_str())?;
    let trace_after_selection = fixture.trace()?;
    if trace_after_selection != trace_before_selection {
        return Err(format!(
            "agent selection changed the fake-provider trace\nbefore:\n{trace_before_selection}\n\
             after:\n{trace_after_selection}"
        )
        .into());
    }
    let actions_after_selection = matched_actions(&fixture)?;
    if actions_after_selection != 4 {
        return Err(format!(
            "agent selection changed matched provider actions from 4 to \
             {actions_after_selection}"
        )
        .into());
    }
    let durable_after = DurableSessionSnapshot::load(&fixture.tau_state(), &session_id)?;
    durable_after.require_prefix(&durable_before_selection)?;
    durable_before_selection.require_prefix(&durable_after)?;
    fixture.complete();
    Ok(())
}

/// Selects both stable IDs and returns semantic transcript rows keyed by ID.
fn select_all_agents(
    terminal: &mut PtyProcess,
    order: [&AgentId; 2],
    identities: &Identities,
    deadline: Instant,
) -> Result<BTreeMap<AgentId, Vec<String>>, Box<dyn std::error::Error>> {
    let mut rows = BTreeMap::new();
    for agent_id in order {
        terminal.send_line(&format!(":agent switch {agent_id}"))?;
        let frame = if agent_id == &identities.main {
            terminal.wait_ready_for(agent_id.as_str(), deadline)?
        } else {
            terminal.wait_for("This active-auto agent is idle", deadline)?
        };
        rows.insert(
            agent_id.clone(),
            assert_transcript_rows(&frame, agent_id, identities)?,
        );
    }
    Ok(rows)
}

/// Prevents replay-triggered provider work, stable-ID transcript mixing, a
/// completed `agent_start` repaint, and targeted worker work reaching the main
/// when the public terminal resumes the closed S8 headless flow.
#[test]
fn public_terminal_cold_resume_selects_main_and_worker() -> Result<(), Box<dyn std::error::Error>> {
    let scenario = scenario();
    let fixture = GateFixture::new_multi_agent(&scenario, Path::new(FAKE_PROVIDER))?;
    let session_id = SessionId::parse(SESSION).expect("known-safe SessionId must be valid");

    let socket_a = fixture.headless_socket();
    let daemon_a = HeadlessProcess::spawn(
        fixture.headless_command(Path::new(HARNESS_DAEMON), &socket_a),
        socket_a.clone(),
        fixture.artifact_path("s8-boot-a-daemon.stderr"),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let mut observer_a = SideObserver::connect(
        &socket_a,
        &session_id,
        fixture.artifact_path("s8-boot-a-observer.json"),
        deadline,
    )?;
    observer_a.wait_for_extension("e2e-fake-provider", deadline)?;
    observer_a.create_main(&session_id, "s8-main", MAIN_PROMPT)?;
    wait_marker(&mut observer_a, "worker completion observed", deadline)?;
    wait_two_idle(&mut observer_a, deadline)?;
    let identities = Identities::from_events(&observer_a.events)?;
    assert_boot_a(&observer_a.events, &session_id, &identities)?;
    assert_exact_ready(&observer_a.events)?;
    assert_provider_turns(
        &observer_a.events,
        &identities,
        ProviderTurns { main: 3, worker: 1 },
    )?;
    let matched_after_a = matched_actions(&fixture)?;
    if matched_after_a != 4 {
        return Err(
            format!("S8 Boot A matched {matched_after_a} scenario actions, expected 4").into(),
        );
    }
    fixture.write_artifact(
        "s8-boot-a-observer.json",
        &serde_json::to_vec_pretty(&observer_a.events)?,
    )?;
    observer_a.disconnect()?;
    drop(observer_a);
    daemon_a.finish()?;
    fixture.require_boot_gone(session_id.as_str())?;

    let snapshot_a = DurableSessionSnapshot::load(&fixture.tau_state(), &session_id)?;
    assert_snapshot_a(&snapshot_a, &identities)?;

    let mut boot_b = PtyProcess::spawn(
        fixture.command(Some(session_id.as_str())),
        true,
        Some(PtyArtifacts::new(
            fixture.artifact_path("s8-boot-b-pty.raw.bounded"),
            fixture.artifact_path("s8-boot-b-pty.normalized.txt"),
        )),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let (socket_b, discovered) =
        discover_daemon(fixture.runtime_home(), Some(&session_id), deadline)?;
    if discovered != session_id {
        return Err("S8 Boot B discovered the wrong session".into());
    }
    let mut observer_b = SideObserver::connect(
        &socket_b,
        &session_id,
        fixture.artifact_path("s8-boot-b-observer.json"),
        deadline,
    )?;
    wait_resume_boundaries(&mut observer_b, &session_id, &identities, deadline)?;
    observer_b.wait_for_extension("e2e-fake-provider", deadline)?;
    assert_replay_only_before_input(&observer_b.events, &session_id, &identities)?;
    if matched_actions(&fixture)? != matched_after_a {
        return Err("S8 cold replay consumed a provider action".into());
    }

    let current = observer_b.roster(&session_id, SessionAgentListScope::Current, deadline)?;
    let history = observer_b.roster(&session_id, SessionAgentListScope::History, deadline)?;
    assert_roster(&current, &identities)?;
    if history != current {
        return Err("S8 current/history directed rosters diverged".into());
    }
    observer_b.drain_available()?;
    assert_final_pre_input_replay(&observer_b.events, &session_id, &identities)?;

    boot_b.send_line(&format!(":agent switch {}", identities.worker))?;
    let restored_worker = boot_b.wait_for("This active-auto agent is idle", deadline)?;
    assert_worker_restored_frame(&restored_worker)?;
    boot_b.require_no_tool_violation()?;

    boot_b.send_line(&format!(":agent switch {}", identities.main))?;
    boot_b.wait_for("worker completion observed", deadline)?;
    let main_frame = boot_b.wait_ready_for(identities.main.as_str(), deadline)?;
    assert_main_terminal_frame(&main_frame)?;
    boot_b.require_no_tool_violation()?;

    boot_b.send_line(&format!(":agent switch {}", identities.worker))?;
    let restored_worker = boot_b.wait_for("This active-auto agent is idle", deadline)?;
    assert_worker_restored_frame(&restored_worker)?;
    boot_b.require_no_tool_violation()?;
    boot_b.send_line(&format!(":agent resume {}", identities.worker))?;
    boot_b.wait_ready_for(identities.worker.as_str(), deadline)?;

    let fresh_start = observer_b.events.len();
    boot_b.send_line("fresh worker work")?;
    wait_agent_marker(
        &mut observer_b,
        &identities.worker,
        "fresh worker complete",
        deadline,
    )?;
    wait_agent_idle(&mut observer_b, &identities.worker, deadline)?;
    boot_b.wait_for("fresh worker complete", deadline)?;
    let final_frame = boot_b.wait_ready_for(identities.worker.as_str(), deadline)?;
    assert_worker_fresh_frame(&final_frame)?;
    boot_b.finish_tool_monitoring()?;
    observer_b.drain_available()?;
    assert_boot_b_live_work(&observer_b.events[fresh_start..], &identities)?;
    assert_provider_turns(
        &observer_b.events,
        &identities,
        ProviderTurns { main: 0, worker: 1 },
    )?;
    assert_exact_ready(&observer_b.events)?;
    assert_exact_consumption(&fixture)?;

    fixture.write_artifact("s8-boot-b-pty.raw.bounded", &boot_b.raw()?)?;
    fixture.write_artifact("s8-boot-b-pty.normalized.txt", final_frame.as_bytes())?;
    fixture.write_artifact(
        "s8-boot-b-observer.json",
        &serde_json::to_vec_pretty(&observer_b.events)?,
    )?;
    drop(observer_b);
    boot_b.finish()?;
    fixture.require_boot_gone(session_id.as_str())?;

    let snapshot_b = DurableSessionSnapshot::load(&fixture.tau_state(), &session_id)?;
    snapshot_b.require_prefix(&snapshot_a)?;
    assert_snapshot_suffix(&snapshot_a, &snapshot_b, &identities)?;
    fixture.complete();
    Ok(())
}

fn scenario() -> ScenarioV2 {
    ScenarioV2::new(
        "s8-public-terminal-main-worker-resume",
        vec![
            ScenarioLaneV2 {
                ctx_id: "s8-main".to_owned(),
                actions: vec![
                    ScenarioActionV2::AgentStartCall {
                        user_text: MAIN_PROMPT.to_owned(),
                        call_id: "s8-agent-start".into(),
                        prompt: WORKER_PROMPT.to_owned(),
                        role: Some("deterministic-worker".to_owned()),
                        task_name: "deterministic worker".to_owned(),
                    },
                    ScenarioActionV2::AgentStartResult {
                        user_text: MAIN_PROMPT.to_owned(),
                        call_id: "s8-agent-start".into(),
                        response: "worker start accepted".to_owned(),
                    },
                    ScenarioActionV2::WatchNotifications {
                        notifications: vec![WatchNotificationV2::Response {
                            content: "worker boot-a complete".to_owned(),
                        }],
                        response: "worker completion observed".to_owned(),
                    },
                ],
            },
            ScenarioLaneV2 {
                ctx_id: "s8-worker".to_owned(),
                actions: vec![
                    ScenarioActionV2::Text {
                        user_text: WORKER_INITIAL.to_owned(),
                        response: "worker boot-a complete".to_owned(),
                    },
                    ScenarioActionV2::Text {
                        user_text: "fresh worker work".to_owned(),
                        response: "fresh worker complete".to_owned(),
                    },
                ],
            },
        ],
    )
}

/// Stable main/worker identities learned from immutable typed creation facts.
struct Identities {
    /// Public main-agent ID used for routing and terminal selection.
    main: AgentId,
    /// Public production-started worker ID used for routing and selection.
    worker: AgentId,
}

impl Identities {
    fn all(&self) -> [&AgentId; 2] {
        [&self.main, &self.worker]
    }
}

/// Exact live provider-turn budget for one observed boot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderTurns {
    /// Accepted main-agent provider prompts.
    main: usize,
    /// Accepted worker-agent provider prompts.
    worker: usize,
}
