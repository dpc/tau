//! S8 public-terminal acceptance over a headless-created main/worker session.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

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
use super::peer_navigation::{
    wait_for_canceled_hold, wait_for_idle_active, wait_for_selected_live_hold,
};
use super::pty_process::{
    PtyArtifacts, PtyProcess, PtyReadGeneration, TerminalSize, VtStyledFrame,
};
use super::{DEADLINE, FAKE_PROVIDER, discover_daemon};

const HARNESS_DAEMON: &str = env!("CARGO_BIN_EXE_tau-e2e-harness-daemon");
const SESSION: &str = "deterministic-e2e-session";
const MAIN_PROMPT: &str = "start the deterministic worker";
const WORKER_PROMPT: &str = "Complete the deterministic worker instruction.";
const WORKER_RESPONSE: &str = "worker boot-a complete";
const MAIN_START_RESPONSE: &str = "worker start accepted";
const MAIN_FINAL_RESPONSE: &str = "worker completion observed";
const HIDDEN_MODEL_SENTINEL: &str = tau_e2e_tests::FAKE_MODEL_ID;
/// Durable instruction text. Typed provenance, rather than marker-shaped text,
/// determines which prefix receives an envelope during provider projection.
const WORKER_INITIAL: &str = concat!(
    "You were started by an agent `main`. Your responses will be delivered to it. ",
    "You can use the `message` tool to communicate with agents.\n\n",
    "Complete the deterministic worker instruction."
);
const WORKER_PROVIDER_INITIAL: &str = concat!(
    "<tau_internal>You were started by an agent `main`. Your responses will be delivered to it. ",
    "You can use the `message` tool to communicate with agents.\n\n</tau_internal>",
    "Complete the deterministic worker instruction."
);
const RESTORE_NOTICE: &str = concat!(
    "Previous session was interrupted and restored. Less than 1 minute has passed ",
    "since the last recorded session event, and the state of the world might have changed. ",
    "Session-scoped tool and extension state may also have changed; inspect current tool state ",
    "and recreate timers or other session-scoped setup if still needed."
);

/// The real CLI traces production-held main and completed-worker journals, then
/// the same live harness routes fresh work to that worker.
#[test]
fn live_cli_trace_preserves_completed_worker_routing() -> Result<(), Box<dyn std::error::Error>> {
    let scenario = live_trace_scenario();
    let fixture = GateFixture::new_multi_agent(&scenario, Path::new(FAKE_PROVIDER))?;
    let session_id = SessionId::parse(SESSION).expect("session id");
    let socket = fixture.headless_socket();
    let daemon = HeadlessProcess::spawn(
        fixture.headless_command(Path::new(HARNESS_DAEMON), &socket),
        socket.clone(),
        fixture.artifact_path("trace-daemon.stderr"),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let mut observer = SideObserver::connect(
        &socket,
        &session_id,
        fixture.artifact_path("trace-observer.json"),
        deadline,
    )?;
    observer.wait_for_extension("e2e-fake-provider", deadline)?;
    observer.create_main(&session_id, "s8-main", MAIN_PROMPT)?;
    wait_marker(&mut observer, MAIN_FINAL_RESPONSE, deadline)?;
    wait_two_idle(&mut observer, deadline)?;
    let identities = Identities::from_events(&observer.events)?;
    for agent_id in identities.all() {
        let checkpoint = fixture
            .tau_state()
            .join("agents")
            .join(agent_id.as_str())
            .join("meta.json");
        while !checkpoint.exists() {
            if Instant::now() >= deadline {
                return Err(
                    format!("production checkpoint was not published for `{agent_id}`").into(),
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    let trace = run_live_trace(&fixture, &identities)?;
    if !trace.contains(identities.main.as_str()) || !trace.contains(identities.worker.as_str()) {
        return Err("live descendant trace omitted main or completed worker".into());
    }

    drop(observer);
    daemon.finish()?;
    fixture.require_boot_gone(session_id.as_str())?;

    let mut terminal = PtyProcess::spawn(
        fixture.command(Some(session_id.as_str())),
        false,
        Some(PtyArtifacts::new(
            fixture.artifact_path("trace-resume.raw.bounded"),
            fixture.artifact_path("trace-resume.normalized.txt"),
        )),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let (socket, discovered) =
        discover_daemon(fixture.runtime_home(), Some(&session_id), deadline)?;
    if discovered != session_id {
        return Err("trace resume discovered the wrong session".into());
    }
    let mut observer = SideObserver::connect(
        &socket,
        &session_id,
        fixture.artifact_path("trace-resume-observer.json"),
        deadline,
    )?;
    wait_resume_boundaries(&mut observer, &session_id, &identities, deadline)?;
    observer.wait_for_extension("e2e-fake-provider", deadline)?;
    terminal.wait_for(MAIN_FINAL_RESPONSE, deadline)?;
    let worker_switch = terminal.read_generation()?;
    terminal.send_line(&format!(":agent switch {}", identities.worker))?;
    wait_for_worker_frame(
        &terminal,
        worker_switch,
        &identities.worker,
        &identities,
        deadline,
    )?;
    terminal.send_line(&format!(":agent resume {}", identities.worker))?;
    terminal.wait_ready_for(identities.worker.as_str(), deadline)?;

    terminal.send_line("held worker work")?;
    let held_prompt = wait_for_selected_live_hold(&mut observer, &identities.worker, deadline)?;
    if !held_prompt.tools.is_empty() {
        return Err("held resumed-worker prompt unexpectedly exposed tools".into());
    }
    let running_trace = run_live_trace(&fixture, &identities)?;
    if !running_trace.contains(identities.worker.as_str()) {
        return Err("running descendant trace omitted the active worker".into());
    }
    observer.cancel_prompt(&session_id, &held_prompt)?;
    wait_for_canceled_hold(&mut observer, &held_prompt, deadline)?;
    wait_for_idle_active(&mut observer, &identities.worker, &held_prompt, deadline)?;

    let fresh_start = observer.events.len();
    terminal.send_line("fresh worker work")?;
    wait_agent_marker(
        &mut observer,
        &identities.worker,
        "fresh worker complete",
        deadline,
    )?;
    wait_agent_idle(&mut observer, &identities.worker, deadline)?;
    terminal.wait_for("fresh worker complete", deadline)?;
    terminal.wait_ready_for(identities.worker.as_str(), deadline)?;
    let fresh_prompts = observer.events[fresh_start..]
        .iter()
        .filter_map(|observed| match &observed.event {
            tau_proto::Event::AgentPromptCreated(prompt)
                if !observed.replay && prompt.agent_id == identities.worker =>
            {
                Some(prompt)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if !matches!(fresh_prompts.as_slice(), [prompt] if prompt.tools.is_empty()) {
        return Err("fresh resumed-worker prompt did not have the sole empty tool snapshot".into());
    }
    assert_provider_turns(
        &observer.events,
        &identities,
        ProviderTurns { main: 0, worker: 2 },
    )?;
    if observer.events.iter().any(|observed| {
        !observed.replay
            && matches!(
                &observed.event,
                tau_proto::Event::ToolRequest(_)
                    | tau_proto::Event::ToolStarted(_)
                    | tau_proto::Event::ToolResultDisplay(_)
                    | tau_proto::Event::ToolError(_)
                    | tau_proto::Event::ProviderToolResult(_)
                    | tau_proto::Event::ProviderToolError(_)
            )
    }) {
        return Err("live-trace companion executed an unexpected tool".into());
    }
    if matched_actions(&fixture)? != 6 {
        return Err("live-trace companion did not consume exactly six actions".into());
    }
    if fixture.trace()?.contains("mismatch") {
        return Err("live-trace companion recorded a provider mismatch".into());
    }

    drop(observer);
    terminal.finish()?;
    fixture.require_boot_gone(session_id.as_str())?;
    fixture.complete();
    Ok(())
}

fn live_trace_scenario() -> ScenarioV2 {
    let mut scenario = scenario();
    scenario.name = "s8-agent-trace-live-descendant-companion".to_owned();
    scenario.lanes[1].actions[1] = ScenarioActionV2::HoldUntilCancel {
        user_text: "held worker work".to_owned(),
        timeout_ms: 10_000,
    };
    scenario.lanes[1].actions.push(ScenarioActionV2::Text {
        user_text: "fresh worker work".to_owned(),
        response: "fresh worker complete".to_owned(),
    });
    scenario
}

fn run_live_trace(
    fixture: &GateFixture,
    identities: &Identities,
) -> Result<String, Box<dyn std::error::Error>> {
    let agents_dir = fixture.tau_state().join("agents");
    let output = fixture
        .command(None)
        .args([
            "agent",
            "trace",
            identities.main.as_str(),
            "--include-descendants",
            "--format",
            "agent-performance-jsonl",
            "--agents-dir",
            agents_dir.to_str().ok_or("non-UTF-8 agents path")?,
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "live trace failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Proves two attached public UIs share ID-keyed semantic transcripts while
/// keeping drafts, selection, themes, redraws, and terminal dimensions local.
#[test]
fn attached_public_terminals_isolate_local_presentation() -> Result<(), Box<dyn std::error::Error>>
{
    let scenario = scenario();
    let fixture = GateFixture::new_multi_agent(&scenario, Path::new(FAKE_PROVIDER))?;
    fixture.enable_prompt_draft_content()?;
    let session_id = SessionId::parse(SESSION).expect("known-safe SessionId must be valid");
    let socket = fixture.headless_socket();
    let daemon = HeadlessProcess::spawn(
        fixture.headless_command(Path::new(HARNESS_DAEMON), &socket),
        socket.clone(),
        fixture.artifact_path("presentation-daemon.stderr"),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let mut observer = SideObserver::connect(
        &socket,
        &session_id,
        fixture.artifact_path("presentation-observer.json"),
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
    let setup_actions = matched_actions(&fixture)?;
    if setup_actions != 4 {
        return Err(format!(
            "presentation fixture consumed {setup_actions} setup actions, expected 4"
        )
        .into());
    }
    observer.disconnect()?;
    drop(observer);
    daemon.finish()?;
    fixture.require_boot_gone(session_id.as_str())?;
    let durable_before = DurableSessionSnapshot::load(&fixture.tau_state(), &session_id)?;
    assert_snapshot_a(&durable_before, &identities)?;

    let mut first = PtyProcess::spawn(
        fixture.command(Some(session_id.as_str())),
        true,
        Some(PtyArtifacts::new(
            fixture.artifact_path("presentation-first.raw.bounded"),
            fixture.artifact_path("presentation-first.normalized.txt"),
        )),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let (live_socket, discovered) =
        discover_daemon(fixture.runtime_home(), Some(&session_id), deadline)?;
    if discovered != session_id {
        return Err(format!(
            "presentation fixture resumed session `{discovered}`, expected `{session_id}`"
        )
        .into());
    }
    let mut observer = SideObserver::connect_observing_prompt_drafts(
        &live_socket,
        &session_id,
        fixture.artifact_path("presentation-observer.json"),
        deadline,
    )?;
    wait_resume_boundaries(&mut observer, &session_id, &identities, deadline)?;
    observer.wait_for_extension("e2e-fake-provider", deadline)?;
    first.wait_for("worker completion observed", deadline)?;
    let mut second = PtyProcess::spawn(
        fixture.attach_command(session_id.as_str()),
        false,
        Some(PtyArtifacts::new(
            fixture.artifact_path("presentation-second.raw.bounded"),
            fixture.artifact_path("presentation-second.normalized.txt"),
        )),
    )?;
    second.wait_for("worker completion observed", deadline)?;
    let trace_before_presentation = fixture.trace()?;
    let observer_before_presentation = observer.events.len();

    const FIRST_DRAFT: &str = "first-ui-private-draft";
    const SECOND_DRAFT: &str = "second-ui-private-draft";
    second.send_text(SECOND_DRAFT)?;
    wait_draft(&mut observer, SECOND_DRAFT, deadline)?;
    let second_draft = second.wait_for(SECOND_DRAFT, deadline)?;
    if second_draft.contains(FIRST_DRAFT) {
        return Err("second UI unexpectedly contained the first UI draft".into());
    }
    let first_after_peer_draft = repaint_barrier(&mut first, "first-peer-draft-barrier", deadline)?;
    if first_after_peer_draft.contains(SECOND_DRAFT) {
        return Err("attached UI draft leaked into the owning resume UI".into());
    }
    first.send_text(FIRST_DRAFT)?;
    wait_draft(&mut observer, FIRST_DRAFT, deadline)?;
    let first_draft = first.wait_for(FIRST_DRAFT, deadline)?;
    if first_draft.contains(SECOND_DRAFT) {
        return Err("attached UI draft leaked into the owning resume UI".into());
    }
    let second_still_private = repaint_barrier(&mut second, "second-peer-draft-barrier", deadline)?;
    if second_still_private.contains(FIRST_DRAFT) {
        return Err("owning UI draft leaked into the attached UI".into());
    }
    first.send_clear_prompt_key()?;
    wait_draft(&mut observer, "", deadline)?;
    first.wait_ready_for(identities.main.as_str(), deadline)?;
    let second_still_private = repaint_barrier(&mut second, "second-peer-clear-barrier", deadline)?;
    if !second_still_private.contains(SECOND_DRAFT) {
        return Err("clearing the owning UI draft cleared the attached UI draft".into());
    }
    if second_still_private.contains(FIRST_DRAFT) {
        return Err("clearing the owning UI draft mutated the attached UI".into());
    }
    second.send_clear_prompt_key()?;
    wait_draft(&mut observer, "", deadline)?;
    second.wait_ready_for(identities.main.as_str(), deadline)?;

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
    let first_worker_switch = first.read_generation()?;
    first.send_line(&format!(":agent switch {}", identities.worker))?;
    wait_for_worker_frame(
        &first,
        first_worker_switch,
        &identities.worker,
        &identities,
        deadline,
    )?;
    second.send_line(":agent")?;
    let second_still_main = second.wait_for(&format!("current: {}", identities.main), deadline)?;
    assert_transcript_rows(&second_still_main, &identities.main, &identities)?;

    let first_worker_switch = first.read_generation()?;
    first.send_line(&format!(":agent switch {}", identities.worker))?;
    wait_for_worker_frame(
        &first,
        first_worker_switch,
        &identities.worker,
        &identities,
        deadline,
    )?;
    let second_worker_switch = second.read_generation()?;
    second.send_line(&format!(":agent switch {}", identities.worker))?;
    wait_for_worker_frame(
        &second,
        second_worker_switch,
        &identities.worker,
        &identities,
        deadline,
    )?;
    let second_main_switch = second.read_generation()?;
    second.send_line(&format!(":agent switch {}", identities.main))?;
    let second_main = second.wait_for_styled_frame_after(
        second_main_switch,
        identities.main.as_str(),
        deadline,
        |frame| assert_transcript_rows(frame, &identities.main, &identities).is_ok(),
    )?;
    let second_style_before = second_main.styles;
    let first_agent_status = first.read_generation()?;
    first.send_line(":agent")?;
    let first_worker = first.wait_for_styled_frame_after(
        first_agent_status,
        identities.worker.as_str(),
        deadline,
        |frame| {
            frame.contains(&format!("current: {}", identities.worker))
                && assert_transcript_rows(frame, &identities.worker, &identities).is_ok()
        },
    )?;
    let first_still_worker = first_worker.frame;
    assert_transcript_rows(&first_still_worker, &identities.worker, &identities)?;

    const SECOND_THEME_NOTICE: &str = "theme set to `tau-dpc` for this UI";
    let first_style_before = first_worker.styles;
    second.send_line(":theme tau-dpc")?;
    let second_themed = second.wait_for(SECOND_THEME_NOTICE, deadline)?;
    assert_transcript_rows(&second_themed, &identities.main, &identities)?;
    let second_theme_change = second.wait_for_marker_style_change(
        identities.main.as_str(),
        &second_style_before,
        deadline,
    )?;
    assert_transcript_rows(&second_theme_change.frame, &identities.main, &identities)?;
    let first_after_local_repaint = styled_repaint_barrier(
        &mut first,
        "first-theme-retention-barrier",
        &identities.worker,
        &identities,
        deadline,
    )?;
    assert_transcript_rows(
        &first_after_local_repaint.frame,
        &identities.worker,
        &identities,
    )?;
    if first_after_local_repaint
        .frame
        .contains(SECOND_THEME_NOTICE)
        || first_after_local_repaint.styles != first_style_before
    {
        return Err("attached UI theme or notice leaked into the owning UI".into());
    }
    let second_stable_theme = styled_repaint_barrier(
        &mut second,
        "second-theme-stability-barrier",
        &identities.main,
        &identities,
        deadline,
    )?;
    if second_stable_theme.styles != second_theme_change.styles {
        return Err("peer repaint changed the themed UI's stable-row style".into());
    }

    let before_worker_switch = second.read_generation()?;
    second.send_line(&format!(":agent switch {}", identities.worker))?;
    let second_worker_wide = second.wait_for_styled_frame_after(
        before_worker_switch,
        identities.worker.as_str(),
        deadline,
        |frame| assert_worker_size_projection(frame, &identities.worker, &identities).is_ok(),
    )?;
    let second_worker_projection =
        assert_worker_size_projection(&second_worker_wide.frame, &identities.worker, &identities)?;
    let second_worker_style = second_worker_wide.styles;
    let narrow_prompt_prefix =
        "You were started by an agent `main`. Your responses will be delivered to it.";
    if !second_worker_wide
        .frame
        .lines()
        .any(|line| line.contains(narrow_prompt_prefix))
    {
        return Err("120x40 worker baseline did not contain the unwrapped prompt prefix".into());
    }
    first.send_line(&format!(":agent switch {}", identities.main))?;
    first.wait_ready_for(identities.main.as_str(), deadline)?;
    let first_before_agent_status = first.read_generation()?;
    first.send_line(":agent")?;
    let first_status = first.wait_for_styled_frame_after(
        first_before_agent_status,
        identities.main.as_str(),
        deadline,
        |frame| {
            frame.contains(&format!("current: {}", identities.main))
                && assert_transcript_rows(frame, &identities.main, &identities).is_ok()
        },
    )?;
    let first_before_resize = first_status.frame;
    let first_main_rows =
        assert_transcript_rows(&first_before_resize, &identities.main, &identities)?;
    let first_main_style = first_status.styles;
    const WIDE_AGENT_USAGE: &str =
        ":agent <new|switch|suspend|resume|auto|name> [agent_id]; current: main; active: 1; known:";
    if !first_before_resize
        .lines()
        .any(|line| line.contains(WIDE_AGENT_USAGE))
    {
        return Err("120x40 peer omitted the unwrapped agent status signature".into());
    }

    let before_resize = second.read_generation()?;
    second.resize(TerminalSize { cols: 72, rows: 24 })?;
    let prefix_compact = narrow_prompt_prefix
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    let second_narrow = second.wait_for_styled_frame_after(
        before_resize,
        identities.worker.as_str(),
        deadline,
        |frame| {
            let compact = frame
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>();
            compact.contains(&prefix_compact)
                && !frame
                    .lines()
                    .any(|line| line.contains(narrow_prompt_prefix))
                && assert_worker_size_projection(frame, &identities.worker, &identities).is_ok()
        },
    )?;
    let narrow_projection =
        assert_worker_size_projection(&second_narrow.frame, &identities.worker, &identities)?;
    if second_worker_projection != narrow_projection {
        return Err(
            format!("72x24 projection changed worker semantics: {narrow_projection:?}").into(),
        );
    }
    if second_narrow.styles != second_worker_style {
        return Err("72x24 resize changed the selected worker's critical status style".into());
    }
    let first_after_peer_resize = styled_repaint_barrier(
        &mut first,
        "wide-peer-resize-barrier",
        &identities.main,
        &identities,
        deadline,
    )?;
    let first_rows_after_resize = assert_transcript_rows(
        &first_after_peer_resize.frame,
        &identities.main,
        &identities,
    )?;
    if first_main_rows != first_rows_after_resize {
        return Err("peer resize changed the 120x40 main transcript projection".into());
    }
    if !first_after_peer_resize
        .frame
        .lines()
        .any(|line| line.contains(WIDE_AGENT_USAGE))
    {
        return Err("peer resize changed the 120x40 unwrapped status signature".into());
    }
    if first_after_peer_resize.styles != first_main_style {
        return Err("peer resize changed the 120x40 selected-main critical status".into());
    }
    let first_final_frame = first_after_peer_resize.frame;

    let post_presentation_roster =
        observer.roster(&session_id, SessionAgentListScope::Current, deadline)?;
    assert_roster(&post_presentation_roster, &identities)?;
    observer.drain_available()?;
    if let Some(offending) =
        observer.events[observer_before_presentation..]
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
            "terminal-local presentation changed provider or agent runtime facts: {offending:?}"
        )
        .into());
    }

    fixture.write_artifact(
        "presentation-observer.json",
        &serde_json::to_vec_pretty(&observer.events)?,
    )?;
    fixture.write_artifact(
        "presentation-first.normalized.txt",
        first_final_frame.as_bytes(),
    )?;
    drop(observer);
    second.finish()?;
    first.finish_exited()?;
    fixture.require_boot_gone(session_id.as_str())?;
    let trace_after_presentation = fixture.trace()?;
    if trace_after_presentation != trace_before_presentation {
        return Err(format!(
            "terminal-local presentation changed the fake-provider trace\nbefore:\n{trace_before_presentation}\n\
             after:\n{trace_after_presentation}"
        )
        .into());
    }
    let actions_after_presentation = matched_actions(&fixture)?;
    if actions_after_presentation != 4 {
        return Err(format!(
            "terminal-local presentation changed matched provider actions from 4 to \
             {actions_after_presentation}"
        )
        .into());
    }
    let durable_after = DurableSessionSnapshot::load(&fixture.tau_state(), &session_id)?;
    durable_after.require_prefix(&durable_before)?;
    assert_presentation_snapshot_suffix(&durable_before, &durable_after, &identities)?;
    fixture.complete();
    Ok(())
}

/// Waits for the exact opt-in source-side prompt-draft liveness fact.
fn wait_draft(
    observer: &mut SideObserver,
    text: &str,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    observer.recv_until(deadline, |observed| {
        matches!(
            &observed.event,
            tau_proto::Event::UiPromptDraft(draft) if draft.text.as_deref() == Some(text)
        )
    })?;
    Ok(())
}

/// Temporarily appends and removes one ASCII canary to prove an exact repaint.
fn repaint_barrier(
    terminal: &mut PtyProcess,
    canary: &str,
    deadline: Instant,
) -> Result<String, Box<dyn std::error::Error>> {
    if !canary.is_ascii() {
        return Err("editor repaint canary must be ASCII".into());
    }
    terminal.send_text(canary)?;
    terminal.wait_for(canary, deadline)?;
    let generation = terminal.read_generation()?;
    terminal.send_backspaces(canary.len())?;
    terminal.wait_for_absence_after(canary, generation, deadline)
}

/// Completes one repaint and returns its transcript and selected-row styles
/// from the same VT observation.
fn styled_repaint_barrier(
    terminal: &mut PtyProcess,
    canary: &str,
    agent_id: &AgentId,
    identities: &Identities,
    deadline: Instant,
) -> Result<VtStyledFrame, Box<dyn std::error::Error>> {
    if !canary.is_ascii() {
        return Err("editor repaint canary must be ASCII".into());
    }
    terminal.send_text(canary)?;
    terminal.wait_for(canary, deadline)?;
    let generation = terminal.read_generation()?;
    terminal.send_backspaces(canary.len())?;
    terminal.wait_for_styled_frame_after(generation, agent_id.as_str(), deadline, |frame| {
        !frame.contains(canary) && assert_transcript_rows(frame, agent_id, identities).is_ok()
    })
}

/// Waits for a newer selected-worker frame with its exact compact idle row and
/// transcript.
fn wait_for_worker_frame(
    terminal: &PtyProcess,
    before_switch: PtyReadGeneration,
    agent_id: &AgentId,
    identities: &Identities,
    deadline: Instant,
) -> Result<String, Box<dyn std::error::Error>> {
    let compact_idle_row = worker_compact_idle_row(agent_id);
    terminal.wait_for_frame_after(before_switch, deadline, |frame| {
        frame.contains(&compact_idle_row)
            && assert_transcript_rows(frame, agent_id, identities).is_ok()
    })
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
        let before_switch = terminal.read_generation()?;
        terminal.send_line(&format!(":agent switch {agent_id}"))?;
        let frame = if agent_id == &identities.main {
            terminal.wait_ready_for(agent_id.as_str(), deadline)?
        } else {
            wait_for_worker_frame(terminal, before_switch, agent_id, identities, deadline)?
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
        false,
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

    let worker_switch = boot_b.read_generation()?;
    boot_b.send_line(&format!(":agent switch {}", identities.worker))?;
    let restored_worker = wait_for_worker_frame(
        &boot_b,
        worker_switch,
        &identities.worker,
        &identities,
        deadline,
    )?;
    assert_worker_restored_frame(&restored_worker)?;
    boot_b.start_tool_monitoring()?;
    boot_b.require_no_tool_violation()?;

    boot_b.send_line(&format!(":agent switch {}", identities.main))?;
    boot_b.wait_for("worker completion observed", deadline)?;
    let main_frame = boot_b.wait_ready_for(identities.main.as_str(), deadline)?;
    assert_main_terminal_frame(&main_frame)?;
    boot_b.require_no_tool_violation()?;

    let worker_switch = boot_b.read_generation()?;
    boot_b.send_line(&format!(":agent switch {}", identities.worker))?;
    let restored_worker = wait_for_worker_frame(
        &boot_b,
        worker_switch,
        &identities.worker,
        &identities,
        deadline,
    )?;
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
                        role: "deterministic-worker".to_owned(),
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
                        user_text: WORKER_PROVIDER_INITIAL.to_owned(),
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
