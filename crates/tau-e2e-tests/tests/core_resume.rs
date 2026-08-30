#![cfg(unix)]

use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tau_e2e_tests::{DurableSnapshot, ScenarioActionV2, ScenarioLaneV2, ScenarioV2};
use tau_proto::{
    AgentId, AgentRuntimeState, CborValue, ContextItem, Event, ModelId, SessionId,
    SessionStartReason, ToolCallId,
};

#[path = "core_resume/attached_dummy_tool.rs"]
mod attached_dummy_tool;
#[path = "core_resume/gate_fixture.rs"]
mod gate_fixture;
#[path = "core_resume/headless_process.rs"]
mod headless_process;
#[path = "core_resume/multi_agent.rs"]
mod multi_agent;
#[path = "core_resume/observer.rs"]
mod observer;
#[path = "core_resume/peer_navigation.rs"]
mod peer_navigation;
#[path = "core_resume/process_group.rs"]
mod process_group;
#[path = "core_resume/pty_process.rs"]
mod pty_process;

use gate_fixture::GateFixture;
use observer::{ObservedEvent, SideObserver, discover_daemon};
use pty_process::{PtyArtifacts, PtyProcess};

const FAKE_PROVIDER: &str = env!("CARGO_BIN_EXE_tau-e2e-fake-provider");
const DEADLINE: Duration = Duration::from_secs(20);
const DUMMY_ROLE: &str = "deterministic-e2e";
const DUMMY_TOOL: &str = "restart_test_dummy";
const HOSTILE_TERMINAL_TEXT: &str = "A\x1b[31mB\x1b[0mC\x1b]52;c;WA==\x07D\u{009B}31mE";

/// The real prompt-stdin process reports unavailable-role admission exactly as
/// stderr diagnostics with empty stdout and a failing exit status.
#[test]
fn prompt_stdin_unavailable_role_exits_without_stdout() -> Result<(), Box<dyn std::error::Error>> {
    let scenario = ScenarioV2::new(
        "prompt-stdin-role-rejection",
        vec![ScenarioLaneV2 {
            ctx_id: "unused".to_owned(),
            actions: vec![ScenarioActionV2::Text {
                user_text: "unused".to_owned(),
                response: "unused".to_owned(),
            }],
        }],
    );
    let fixture = GateFixture::new(&scenario, Path::new(FAKE_PROVIDER))?;
    let mut command = fixture.command(None);
    command
        .arg("--prompt-stdin")
        .arg("--role")
        .arg("missing-role")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .expect("prompt stdin")
        .write_all(b"hello\n")?;
    let output = child.wait_with_output()?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.lines().any(|line| line.starts_with("session_id: ")));
    assert!(stderr.lines().any(|line| line == "role: missing-role"));
    assert!(stderr.contains(
        "error: create-agent request failed (role_unavailable): unknown role `missing-role`"
    ));
    fixture.complete();
    Ok(())
}

/// The real prompt-stdin process follows accepted admission through provider
/// completion and writes only the correlated assistant text to stdout.
#[test]
fn prompt_stdin_accepted_prompt_prints_correlated_completion()
-> Result<(), Box<dyn std::error::Error>> {
    let scenario = ScenarioV2::new(
        "prompt-stdin-success",
        vec![ScenarioLaneV2 {
            ctx_id: "dynamic-ui-prompt".to_owned(),
            actions: vec![ScenarioActionV2::Text {
                user_text: "<user>hello from prompt stdin\n</user>".to_owned(),
                response: "correlated completion".to_owned(),
            }],
        }],
    );
    let fixture = GateFixture::new(&scenario, Path::new(FAKE_PROVIDER))?;
    let mut command = fixture.command(None);
    command
        .arg("--prompt-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .expect("prompt stdin")
        .write_all(b"hello from prompt stdin\n")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout)?, "correlated completion\n");
    fixture.complete();
    Ok(())
}

/// A real process with piped descriptors preserves every provider-authored
/// semantic UTF-8 byte and the existing trailing LF for machine consumers.
#[test]
fn prompt_stdin_piped_output_preserves_terminal_control_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    let scenario = ScenarioV2::new(
        "prompt-stdin-piped-terminal-controls",
        vec![ScenarioLaneV2 {
            ctx_id: "dynamic-ui-prompt".to_owned(),
            actions: vec![ScenarioActionV2::Text {
                user_text: "<user>piped terminal controls\n</user>".to_owned(),
                response: HOSTILE_TERMINAL_TEXT.to_owned(),
            }],
        }],
    );
    let fixture = GateFixture::new(&scenario, Path::new(FAKE_PROVIDER))?;
    let mut command = fixture.command(None);
    command
        .arg("--prompt-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .expect("prompt stdin")
        .write_all(b"piped terminal controls\n")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output.stdout,
        format!("{HOSTILE_TERMINAL_TEXT}\n").as_bytes()
    );
    fixture.complete();
    Ok(())
}

/// A real PTY makes inherited stdout terminal-bound, so prompt-stdin removes
/// ESC CSI, OSC/BEL, and C1 CSI while retaining the readable answer and LF.
#[test]
fn prompt_stdin_pty_output_sanitizes_terminal_control_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    let scenario = ScenarioV2::new(
        "prompt-stdin-pty-terminal-controls",
        vec![ScenarioLaneV2 {
            ctx_id: "dynamic-ui-prompt".to_owned(),
            actions: vec![ScenarioActionV2::Text {
                user_text: "<user>pty terminal controls\n</user>".to_owned(),
                response: HOSTILE_TERMINAL_TEXT.to_owned(),
            }],
        }],
    );
    let fixture = GateFixture::new(&scenario, Path::new(FAKE_PROVIDER))?;
    let mut command = fixture.command(None);
    command.arg("--prompt-stdin");
    let mut process = PtyProcess::spawn(command, false, None)?;
    process.send_text("pty terminal controls\n\u{4}")?;
    process.wait_for("ABCDE", Instant::now() + DEADLINE)?;
    let raw = process.finish_exited()?;
    assert!(
        raw.windows(b"ABCDE\r\n".len())
            .any(|bytes| bytes == b"ABCDE\r\n")
    );
    assert!(!raw.contains(&0x1b));
    assert!(!raw.contains(&0x07));
    assert!(!raw.windows(2).any(|bytes| bytes == b"\xc2\x9b"));
    assert!(!raw.windows(b"WA==".len()).any(|bytes| bytes == b"WA=="));
    fixture.complete();
    Ok(())
}

/// `--prompt-stdin` must forward colon-prefixed input as literal user text so
/// the initial agent prompt cannot be consumed by the command dispatcher.
#[test]
fn prompt_stdin_forwards_colon_prefixed_input_literally() -> Result<(), Box<dyn std::error::Error>>
{
    let scenario = ScenarioV2::new(
        "prompt-stdin-literal-colon",
        vec![ScenarioLaneV2 {
            ctx_id: "dynamic-ui-prompt".to_owned(),
            actions: vec![ScenarioActionV2::Text {
                user_text: "<user>:skill\n</user>".to_owned(),
                response: "literal colon prompt".to_owned(),
            }],
        }],
    );
    let fixture = GateFixture::new(&scenario, Path::new(FAKE_PROVIDER))?;
    let mut command = fixture.command(None);
    command
        .arg("--prompt-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .expect("prompt stdin")
        .write_all(b":skill\n")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout)?, "literal colon prompt\n");
    fixture.complete();
    Ok(())
}

/// The real prompt-stdin process remains attached after accepted admission and
/// converts its correlated provider terminal into a nonzero, stdout-free exit.
#[test]
fn prompt_stdin_accepted_provider_failure_exits_without_stdout()
-> Result<(), Box<dyn std::error::Error>> {
    let scenario = ScenarioV2::new(
        "prompt-stdin-provider-failure",
        vec![ScenarioLaneV2 {
            ctx_id: "dynamic-ui-prompt".to_owned(),
            actions: vec![ScenarioActionV2::Error {
                user_text: "<user>fail after admission\n</user>".to_owned(),
                failure_kind: tau_proto::ProviderFailureKind::Unknown,
                error: "synthetic accepted failure".to_owned(),
            }],
        }],
    );
    let fixture = GateFixture::new(&scenario, Path::new(FAKE_PROVIDER))?;
    let mut command = fixture.command(None);
    command
        .arg("--prompt-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .expect("prompt stdin")
        .write_all(b"fail after admission\n")?;
    let output = child.wait_with_output()?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.contains("synthetic accepted failure"));
    fixture.complete();
    Ok(())
}

/// Proves a second exact public CLI attached after one complete turn presents
/// current state before catch-up transcript while protocol catch-up keeps its
/// historical-first delivery order and attachment spends no provider action.
#[test]
fn late_attached_public_pty_stages_current_state_before_completed_turn()
-> Result<(), Box<dyn std::error::Error>> {
    let nonce = format!("{:x}", std::process::id());
    let prompt = format!("attach-parity-prompt-{nonce}");
    let response = format!("attach-parity-response-{nonce}");
    let scenario = ScenarioV2::new(
        "live-dual-pty-attach",
        vec![ScenarioLaneV2 {
            ctx_id: "dynamic-ui-prompt".to_owned(),
            actions: vec![ScenarioActionV2::Text {
                user_text: format!("<user>{prompt}</user>"),
                response: response.clone(),
            }],
        }],
    );
    let fixture = GateFixture::new(&scenario, Path::new(FAKE_PROVIDER))?;
    let mut original = PtyProcess::spawn(
        fixture.command(None),
        false,
        Some(PtyArtifacts::new(
            fixture.artifact_path("attach-original.raw.bounded"),
            fixture.artifact_path("attach-original.normalized.txt"),
        )),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let (socket, session_id) = discover_daemon(fixture.runtime_home(), None, deadline)?;
    let mut observer = SideObserver::connect(
        &socket,
        &session_id,
        fixture.artifact_path("attach-observer.json"),
        deadline,
    )?;
    wait_extensions(&mut observer, deadline)?;
    wait_for_dummy_role_selection(&mut observer, deadline)?;
    original.wait_ready_to_start_role(DUMMY_ROLE, deadline)?;

    original.send_line(&prompt)?;
    let loaded = observer.recv_until(deadline, |observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::SessionAgentLoaded(loaded) if loaded.session_id == session_id
            )
    })?;
    let Event::SessionAgentLoaded(loaded) = loaded.event else {
        unreachable!("predicate admitted another event")
    };
    let agent_id = loaded.agent_id;
    observer.recv_until(deadline, |observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::ProviderResponseFinished(finished)
                    if finished.agent_id == agent_id
                        && provider_finished_contains(&observed.event, &response)
            )
    })?;
    observer.recv_until(deadline, |observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::AgentStatsUpdated(stats)
                    if stats.agent_id == agent_id
                        && stats.runtime_state == AgentRuntimeState::Idle
            )
    })?;
    original.wait_for(&response, deadline)?;
    let mut catch_up_observer = SideObserver::connect(
        &socket,
        &session_id,
        fixture.artifact_path("attach-catch-up-observer.json"),
        deadline,
    )?;
    catch_up_observer.recv_until(deadline, |observed| {
        matches!(
            &observed.event,
            Event::SessionReplayComplete(complete) if complete.session_id == session_id
        )
    })?;
    assert_protocol_catch_up_order(
        &catch_up_observer.events,
        &session_id,
        &prompt,
        &response,
        &agent_id,
    )?;

    let attached = PtyProcess::spawn(
        fixture.attach_command(session_id.as_str()),
        false,
        Some(PtyArtifacts::new(
            fixture.artifact_path("attach-second.raw.bounded"),
            fixture.artifact_path("attach-second.normalized.txt"),
        )),
    )?;
    attached.wait_for(&response, deadline)?;
    let original_frame = original.wait_ready_for(agent_id.as_str(), deadline)?;
    let attached_frame = attached.wait_ready_for(agent_id.as_str(), deadline)?;
    assert_attach_semantics(&original_frame, &session_id, &prompt, &response, &agent_id)?;
    assert_attach_semantics(&attached_frame, &session_id, &prompt, &response, &agent_id)?;
    assert_exact_ready_set(&observer.events)?;
    let matched = fixture
        .trace()?
        .lines()
        .filter(|line| line.contains(" matched "))
        .count();
    assert_eq!(matched, 1, "expected exactly one fake-provider action");

    fixture.write_artifact("attach-original.normalized.txt", original_frame.as_bytes())?;
    fixture.write_artifact("attach-second.normalized.txt", attached_frame.as_bytes())?;
    fixture.write_artifact(
        "attach-observer.json",
        &serde_json::to_vec_pretty(&observer.events)?,
    )?;
    fixture.write_artifact(
        "attach-catch-up-observer.json",
        &serde_json::to_vec_pretty(&catch_up_observer.events)?,
    )?;
    drop(catch_up_observer);
    drop(observer);
    attached.finish()?;
    original.finish_exited()?;
    fixture.require_boot_gone(session_id.as_str())?;
    fixture.complete();
    Ok(())
}

/// Public PTYs attached to a supervised persistent session keep UI exit
/// separate from session shutdown: `:quit` removes only one attachment, while
/// `:quit-session` terminates the daemon, every remaining UI, and its runtime
/// discovery artifacts.
#[test]
fn attached_quit_is_local_and_quit_session_is_global() -> Result<(), Box<dyn std::error::Error>> {
    let scenario = ScenarioV2::new(
        "attached-ui-session-lifetime",
        vec![ScenarioLaneV2 {
            ctx_id: "unused-lifetime-lane".to_owned(),
            actions: vec![ScenarioActionV2::Text {
                user_text: "<user>unused lifetime prompt</user>".to_owned(),
                response: "unused lifetime response".to_owned(),
            }],
        }],
    );
    let fixture = GateFixture::new(&scenario, Path::new(FAKE_PROVIDER))?;
    let session_id = SessionId::parse("attached-ui-lifetime")?;
    let mut serve_command = fixture.command(None);
    serve_command
        .arg("serve")
        .arg("--session")
        .arg(session_id.as_str())
        .arg("--create")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut serve = serve_command.spawn()?;
    let deadline = Instant::now() + DEADLINE;
    let (socket, discovered_session_id) =
        discover_daemon(fixture.runtime_home(), Some(&session_id), deadline)?;
    assert_eq!(discovered_session_id, session_id);
    let mut observer = SideObserver::connect(
        &socket,
        &session_id,
        fixture.artifact_path("attached-lifetime-observer.json"),
        deadline,
    )?;
    wait_extensions(&mut observer, deadline)?;
    wait_for_dummy_role_selection(&mut observer, deadline)?;

    let mut quitter = PtyProcess::spawn(fixture.attach_command(session_id.as_str()), false, None)?;
    let mut shutdown_requester =
        PtyProcess::spawn(fixture.attach_command(session_id.as_str()), false, None)?;
    quitter.wait_ready_to_start_role(DUMMY_ROLE, deadline)?;
    shutdown_requester.wait_ready_to_start_role(DUMMY_ROLE, deadline)?;

    quitter.send_line(":quit")?;
    quitter.finish_exited()?;
    let (_still_live_socket, still_live_session) =
        discover_daemon(fixture.runtime_home(), Some(&session_id), deadline)?;
    assert_eq!(still_live_session, session_id);

    shutdown_requester.send_line(":quit-session")?;
    shutdown_requester.finish_exited()?;
    drop(observer);

    let serve_status = serve.wait()?;
    assert!(
        serve_status.success(),
        "supervised serve exited with {serve_status}"
    );
    fixture.require_boot_gone(session_id.as_str())?;
    fixture.complete();
    Ok(())
}

/// Proves a second public CLI attached only after a correlated provider hold is
/// ready presents the same selected agent, then both terminals settle after one
/// exact cancellation while typed stats prove the running-to-idle transition.
/// Requester-directed cancellation feedback stays on the side observer that
/// initiated the action rather than being broadcast to either terminal.
#[test]
fn live_attached_public_ptys_share_selected_agent_and_cancellation_settlement()
-> Result<(), Box<dyn std::error::Error>> {
    let nonce = format!("{:x}", std::process::id());
    let prompt_text = format!("attach-hold-prompt-{nonce}");
    let scenario = ScenarioV2::new(
        "live-dual-pty-attach",
        vec![ScenarioLaneV2 {
            ctx_id: "dynamic-ui-prompt".to_owned(),
            actions: vec![ScenarioActionV2::HoldUntilCancel {
                user_text: format!("<user>{prompt_text}</user>"),
                timeout_ms: 10_000,
            }],
        }],
    );
    let fixture = GateFixture::new(&scenario, Path::new(FAKE_PROVIDER))?;
    let mut original = PtyProcess::spawn(
        fixture.command(None),
        false,
        Some(PtyArtifacts::new(
            fixture.artifact_path("attach-cancel-original.raw.bounded"),
            fixture.artifact_path("attach-cancel-original.normalized.txt"),
        )),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let (socket, session_id) = discover_daemon(fixture.runtime_home(), None, deadline)?;
    let mut observer = SideObserver::connect(
        &socket,
        &session_id,
        fixture.artifact_path("attach-cancel-observer.json"),
        deadline,
    )?;
    wait_extensions(&mut observer, deadline)?;
    wait_for_dummy_role_selection(&mut observer, deadline)?;
    original.wait_ready_to_start_role(DUMMY_ROLE, deadline)?;
    original.send_line(&prompt_text)?;
    let loaded = observer.recv_until(deadline, |observed| {
        matches!(
            &observed.event,
            Event::SessionAgentLoaded(loaded) if loaded.session_id == session_id
        )
    })?;
    let Event::SessionAgentLoaded(loaded) = loaded.event else {
        unreachable!("predicate admitted another event")
    };
    let agent_id = loaded.agent_id;
    let prompt = peer_navigation::wait_for_selected_live_hold(&mut observer, &agent_id, deadline)?;
    original.wait_for(&peer_navigation::hold_ready_notice(&prompt), deadline)?;
    peer_navigation::assert_hold_live(&fixture, &prompt)?;

    let attached = PtyProcess::spawn(
        fixture.attach_command(session_id.as_str()),
        false,
        Some(PtyArtifacts::new(
            fixture.artifact_path("attach-cancel-second.raw.bounded"),
            fixture.artifact_path("attach-cancel-second.normalized.txt"),
        )),
    )?;
    let original_running = original.wait_for(&format!("@{}", agent_id.as_str()), deadline)?;
    let attached_running = attached.wait_for(&format!("@{}", agent_id.as_str()), deadline)?;
    assert_live_attach_semantics(&original_running, &session_id, &agent_id)?;
    assert_live_attach_semantics(&attached_running, &session_id, &agent_id)?;

    observer.cancel_prompt(&session_id, &prompt)?;
    peer_navigation::wait_for_canceled_hold(&mut observer, &prompt, deadline)?;
    peer_navigation::assert_hold_reaped(&fixture, &prompt)?;
    peer_navigation::wait_for_selected_idle(&mut observer, &agent_id, &prompt, deadline)?;
    let original_idle = original.wait_ready_for(agent_id.as_str(), deadline)?;
    let attached_idle = attached.wait_ready_for(agent_id.as_str(), deadline)?;
    peer_navigation::assert_exact_canceled_hold_facts(&observer.events, &prompt)?;
    assert_settled_attach_semantics(&original_idle, &session_id, &agent_id)?;
    assert_settled_attach_semantics(&attached_idle, &session_id, &agent_id)?;

    fixture.write_artifact(
        "attach-cancel-observer.json",
        &serde_json::to_vec_pretty(&observer.events)?,
    )?;
    fixture.write_artifact(
        "attach-cancel-original.normalized.txt",
        original_idle.as_bytes(),
    )?;
    fixture.write_artifact(
        "attach-cancel-second.normalized.txt",
        attached_idle.as_bytes(),
    )?;
    drop(observer);
    attached.finish()?;
    original.finish_exited()?;
    fixture.require_boot_gone(session_id.as_str())?;
    fixture.complete();
    Ok(())
}

/// Checks the stable session, selected agent, and editable idle status after
/// cancellation without depending on transcript rows retained in the viewport.
fn assert_settled_attach_semantics(
    frame: &str,
    session_id: &SessionId,
    agent_id: &AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    for needle in [
        format!("sessions/{}/", session_id.as_str()),
        format!("Write a message to {}...", agent_id.as_str()),
        format!("@{}", agent_id.as_str()),
    ] {
        if !frame.lines().any(|row| row.contains(&needle)) {
            return Err(format!("missing settled semantic row `{needle}` in:\n{frame}").into());
        }
    }
    Ok(())
}

/// Checks stable selected-agent and session status semantics shared by both
/// live attached terminal projections; typed observer stats own runtime.
fn assert_live_attach_semantics(
    frame: &str,
    session_id: &SessionId,
    agent_id: &AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    for needle in [
        format!("sessions/{}/", session_id.as_str()),
        format!("@{}", agent_id.as_str()),
    ] {
        if !frame.lines().any(|row| row.contains(&needle)) {
            return Err(format!("missing semantic row `{needle}` in:\n{frame}").into());
        }
    }
    Ok(())
}

/// Proves Boot A selects `deterministic-e2e` / `fake/test`, exposes only
/// `restart_test_dummy`, and renders the matching editable prompt before first
/// input. It then checks the restored terminal row, rejects pending repaints
/// during the fresh turn, and proves the same durable agent remains useful
/// after cold resume.
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
        Some(PtyArtifacts::new(
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
    wait_for_dummy_role_selection(&mut observer_a, deadline)?;
    boot_a.wait_ready_to_start_role(DUMMY_ROLE, deadline)?;
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
        false,
        Some(PtyArtifacts::new(
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
    boot_b.start_tool_monitoring()?;

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

/// Checks stable semantic row classes and their partial order without requiring
/// byte-for-byte or cell-for-cell equality between attached terminal views.
fn assert_attach_semantics(
    frame: &str,
    session_id: &SessionId,
    prompt: &str,
    response: &str,
    agent_id: &AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    let rows = frame.lines().collect::<Vec<_>>();
    let find = |needle: &str| {
        rows.iter()
            .position(|row| row.contains(needle))
            .ok_or_else(|| format!("missing semantic row `{needle}` in:\n{frame}"))
    };
    let session = find(&format!("sessions/{}/", session_id.as_str()))?;
    let extension = find("extension e2e-fake-provider ready")?;
    let submitted = find(prompt)?;
    let initialized = find(&format!("initialized {}", agent_id.as_str()))?;
    let answered = find(response)?;
    let editable = find(&format!("Write a message to {}...", agent_id.as_str()))?;
    let status = find(&format!("@{}", agent_id.as_str()))?;
    if !(session < submitted
        && extension < submitted
        && initialized <= submitted
        && submitted < answered
        && answered < editable
        && editable <= status)
    {
        return Err(format!(
            "semantic row order violated: session={session}, extension={extension}, \
             initialized={initialized}, submitted={submitted}, answered={answered}, \
              editable={editable}, status={status}\n{frame}"
        )
        .into());
    }
    Ok(())
}

/// Confirms the harness keeps its canonical catch-up order; only the attached
/// CLI may move visible transcript behind the later current-state snapshots.
fn assert_protocol_catch_up_order(
    events: &[ObservedEvent],
    session_id: &SessionId,
    prompt: &str,
    response: &str,
    agent_id: &AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    let prompt = events.iter().position(|observed| {
        observed.replay
            && matches!(
                &observed.event,
                Event::AgentPromptSubmitted(submitted) if submitted.text == prompt
            )
    });
    let response = events.iter().position(|observed| {
        observed.replay && provider_finished_contains(&observed.event, response)
    });
    let session = events.iter().position(|observed| {
        observed.replay
            && matches!(
                &observed.event,
                Event::SessionStarted(started) if &started.session_id == session_id
            )
    });
    let complete = events.iter().position(|observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::SessionReplayComplete(value) if &value.session_id == session_id
            )
    });
    let extension = events.iter().position(|observed| {
        observed.replay
            && matches!(
                &observed.event,
                Event::ExtensionReady(ready)
                    if ready.extension_name.as_str() == "e2e-fake-provider"
            )
    });
    let initialized = events.iter().position(|observed| {
        observed.replay
            && matches!(
                &observed.event,
                Event::HarnessAgentContextInitialized(value)
                    if &value.agent_id == agent_id
            )
    });
    let (
        Some(prompt),
        Some(response),
        Some(session),
        Some(extension),
        Some(initialized),
        Some(complete),
    ) = (prompt, response, session, extension, initialized, complete)
    else {
        return Err("catch-up observer missed transcript, state, or replay boundary".into());
    };
    if !(session < prompt
        && prompt < response
        && response < extension
        && extension < initialized
        && initialized < complete)
    {
        return Err(format!(
            "protocol catch-up order changed: prompt={prompt}, response={response}, \
             session={session}, extension={extension}, initialized={initialized}, \
             complete={complete}"
        )
        .into());
    }
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

/// Waits for the exact dummy-tool role snapshot and its resolved model
/// selection so Boot A cannot accept input from the CLI's fallback role.
fn wait_for_dummy_role_selection(
    observer: &mut SideObserver,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let available = if let Some(available) =
        observer
            .events
            .iter()
            .find_map(|observed| match &observed.event {
                Event::HarnessRolesAvailable(available)
                    if available.roles.iter().any(|role| role.name == DUMMY_ROLE) =>
                {
                    Some(available.clone())
                }
                _ => None,
            }) {
        available
    } else {
        let observed = observer.recv_until(deadline, |observed| {
            matches!(&observed.event, Event::HarnessRolesAvailable(available)
                if available.roles.iter().any(|role| role.name == DUMMY_ROLE))
        })?;
        match observed.event {
            Event::HarnessRolesAvailable(available) => available,
            _ => unreachable!("predicate admitted another event"),
        }
    };
    let role = available
        .roles
        .iter()
        .find(|role| role.name == DUMMY_ROLE)
        .ok_or("Boot A omitted its deterministic role")?;
    let tools = role
        .details
        .as_ref()
        .and_then(|details| details.tools.as_ref())
        .ok_or("Boot A deterministic role omitted its explicit tool snapshot")?;
    if tools.len() != 1 || tools[0].as_str() != DUMMY_TOOL {
        return Err(format!(
            "Boot A `{DUMMY_ROLE}` tool snapshot was {tools:?}, expected only `{DUMMY_TOOL}`"
        )
        .into());
    }

    let expected_model = ModelId::from("fake/test");
    let selected = |observed: &ObservedEvent| {
        matches!(&observed.event, Event::HarnessRoleSelected(selected)
            if selected.role == DUMMY_ROLE
                && selected.model.as_ref() == Some(&expected_model))
    };
    if !observer.events.iter().any(selected) {
        observer.recv_until(deadline, selected)?;
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
            Event::ToolResultDisplay(result)
                if &result.call_id == call_id
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
                Event::ToolResultDisplay(value)
                    if &value.call_id == call_id
                        && value.tool_name.as_str() == "restart_test_dummy"
                        && value.tool_type == tau_proto::ToolType::Function
                        && value.kind == tau_proto::ToolResultKind::Final
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
                    Event::ToolResultDisplay(value) if &value.call_id == call_id
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
                Event::ToolResultDisplay(value) => &value.call_id == call_id,
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
