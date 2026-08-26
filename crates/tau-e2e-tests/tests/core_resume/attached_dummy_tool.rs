//! Deterministic dual-PTY parity scenarios for dummy-tool presentation.

use std::path::Path;
use std::time::{Duration, Instant};

use tau_e2e_tests::{DurableSnapshot, ScenarioActionV2, ScenarioLaneV2, ScenarioV2};
use tau_proto::{Event, ToolCallId};

use super::{
    DEADLINE, DUMMY_ROLE, FAKE_PROVIDER, GateFixture, PtyArtifacts, PtyProcess, SideObserver,
    assert_durable_tool, assert_exact_ready_set, assert_tool_admission, discover_daemon,
    provider_finished_contains, unique_tool_row, wait_extensions, wait_for_agent,
    wait_for_dummy_role_selection, wait_for_terminal_turn,
};

/// Attaching after a completed dummy round preserves the exact terminal tool
/// row in both public terminal projections without spending another action.
#[test]
fn attach_after_completed_dummy_round_preserves_exact_terminal_row()
-> Result<(), Box<dyn std::error::Error>> {
    let ids = ScenarioIds::new("completed");
    let scenario = ids.scenario();
    let fixture = GateFixture::new(&scenario, Path::new(FAKE_PROVIDER))?;
    let (original, mut observer, session_id, agent_id, deadline) =
        start_tool_round(&fixture, &ids)?;
    wait_for_terminal_turn(
        &mut observer,
        &agent_id,
        &ids.call_id,
        &ids.completion,
        deadline,
    )?;
    original.wait_for(&ids.completion, deadline)?;

    let attached = PtyProcess::spawn(
        fixture.attach_command(session_id.as_str()),
        false,
        Some(PtyArtifacts::new(
            fixture.artifact_path("attach-tool-complete-second.raw.bounded"),
            fixture.artifact_path("attach-tool-complete-second.normalized.txt"),
        )),
    )?;
    original.wait_for("restart_test_dummy", deadline)?;
    attached.wait_for("restart_test_dummy", deadline)?;
    attached.wait_for(&ids.completion, deadline)?;
    let original_frame = original.wait_ready_for(agent_id.as_str(), deadline)?;
    let attached_frame = attached.wait_ready_for(agent_id.as_str(), deadline)?;
    let original_row = unique_tool_row(&original_frame)?;
    let attached_row = unique_tool_row(&attached_frame)?;
    assert_eq!(
        normalized_tool_row(original_row),
        normalized_tool_row(attached_row)
    );
    assert!(original_row.contains("ok"));
    assert!(!original_row.contains("pending"));

    assert_round_facts(&fixture, &observer, &session_id, &agent_id, &ids, deadline)?;
    finish_pair(fixture, observer, attached, original, &session_id)
}

/// Attaching while a dummy invocation is pending preserves pending-row parity;
/// after one release, both terminal snapshots show the same successful row.
#[test]
fn attach_during_dummy_round_preserves_pending_and_terminal_parity()
-> Result<(), Box<dyn std::error::Error>> {
    let ids = ScenarioIds::new("pending");
    let scenario = ids.scenario();
    let fixture = GateFixture::new_with_dummy_release(&scenario, Path::new(FAKE_PROVIDER))?;
    let (original, mut observer, session_id, agent_id, deadline) =
        start_tool_round(&fixture, &ids)?;
    observer.recv_until(deadline, |observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::ToolProgressReported(progress)
                    if progress.call_id == ids.call_id
                        && progress.message.as_deref()
                            == Some("hold_until_success_release ready")
            )
    })?;
    let pending_snapshot =
        wait_for_durable_snapshot(&fixture, &session_id, deadline, |snapshot| {
            pending_durable_counts(snapshot, &ids.call_id) == (1, 0)
        })?;
    assert_pending_durable(&pending_snapshot, &ids.call_id)?;

    let attached = PtyProcess::spawn(
        fixture.attach_command(session_id.as_str()),
        false,
        Some(PtyArtifacts::new(
            fixture.artifact_path("attach-tool-pending-second.raw.bounded"),
            fixture.artifact_path("attach-tool-pending-second.normalized.txt"),
        )),
    )?;
    let original_pending = original.wait_for("restart_test_dummy", deadline)?;
    let attached_pending = attached.wait_for("restart_test_dummy", deadline)?;
    let original_pending_row = unique_tool_row(&original_pending)?.to_owned();
    let attached_pending_row = unique_tool_row(&attached_pending)?.to_owned();
    assert_eq!(
        normalized_tool_row(&original_pending_row),
        normalized_tool_row(&attached_pending_row)
    );
    assert!(!original_pending_row.contains("ok"));

    fixture.release_dummy(&ids.call_id)?;
    wait_for_terminal_turn(
        &mut observer,
        &agent_id,
        &ids.call_id,
        &ids.completion,
        deadline,
    )?;
    let original_done = original.wait_for(&ids.completion, deadline)?;
    let attached_done = attached.wait_for(&ids.completion, deadline)?;
    let original_done_row = unique_tool_row(&original_done)?;
    let attached_done_row = unique_tool_row(&attached_done)?;
    assert_eq!(
        normalized_tool_row(original_done_row),
        normalized_tool_row(attached_done_row)
    );
    assert!(original_done_row.contains("ok"));
    assert_ne!(original_pending_row, original_done_row);

    assert_round_facts(&fixture, &observer, &session_id, &agent_id, &ids, deadline)?;
    finish_pair(fixture, observer, attached, original, &session_id)
}

/// Removes only the volatile whole-second duration chip from an otherwise exact
/// normalized semantic tool row.
fn normalized_tool_row(row: &str) -> String {
    row.split_whitespace()
        .filter(|token| {
            let Some(seconds) = token.strip_suffix('s') else {
                return true;
            };
            seconds.is_empty() || !seconds.bytes().all(|byte| byte.is_ascii_digit())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Unique identities and exact fake-provider scenario for one tool round.
struct ScenarioIds {
    /// Submitted user prompt.
    prompt: String,
    /// Provider-issued tool correlation.
    call_id: ToolCallId,
    /// Provider continuation marker.
    completion: String,
}

impl ScenarioIds {
    /// Creates process-unique identities for one independently runnable test.
    fn new(label: &str) -> Self {
        let nonce = format!("{label}-{:x}", std::process::id());
        Self {
            prompt: format!("attach-tool-prompt-{nonce}"),
            call_id: ToolCallId::from(format!("attach-tool-call-{nonce}")),
            completion: format!("attach-tool-complete-{nonce}"),
        }
    }

    /// Builds the exact call/result continuation lane.
    fn scenario(&self) -> ScenarioV2 {
        ScenarioV2::new(
            "live-dual-pty-attach",
            vec![ScenarioLaneV2 {
                ctx_id: self.prompt.clone(),
                actions: vec![
                    ScenarioActionV2::DummyToolCall {
                        user_text: self.prompt.clone(),
                        call_id: self.call_id.clone(),
                    },
                    ScenarioActionV2::DummyToolResult {
                        user_text: self.prompt.clone(),
                        call_id: self.call_id.clone(),
                        response: self.completion.clone(),
                    },
                ],
            }],
        )
    }
}

type StartedRound = (
    PtyProcess,
    SideObserver,
    tau_proto::SessionId,
    tau_proto::AgentId,
    Instant,
);

/// Starts one exact universal CLI and waits through durable tool admission.
fn start_tool_round(
    fixture: &GateFixture,
    ids: &ScenarioIds,
) -> Result<StartedRound, Box<dyn std::error::Error>> {
    let mut original = PtyProcess::spawn(
        fixture.command(None),
        false,
        Some(PtyArtifacts::new(
            fixture.artifact_path("attach-tool-original.raw.bounded"),
            fixture.artifact_path("attach-tool-original.normalized.txt"),
        )),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let (socket, session_id) = discover_daemon(fixture.runtime_home(), None, deadline)?;
    let mut observer = SideObserver::connect(
        &socket,
        &session_id,
        fixture.artifact_path("attach-tool-observer.json"),
        deadline,
    )?;
    wait_extensions(&mut observer, deadline)?;
    wait_for_dummy_role_selection(&mut observer, deadline)?;
    original.wait_ready_to_start_role(DUMMY_ROLE, deadline)?;
    original.send_line(&ids.prompt)?;
    let agent_id = wait_for_agent(&mut observer, &session_id, deadline)?;
    observer.recv_until(deadline, |observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::ToolRequest(request) if request.call_id == ids.call_id
            )
    })?;
    Ok((original, observer, session_id, agent_id, deadline))
}

/// Proves the durable pending cut contains the call but no result or provider
/// continuation before the external release.
fn assert_pending_durable(
    snapshot: &DurableSnapshot,
    call_id: &ToolCallId,
) -> Result<(), Box<dyn std::error::Error>> {
    let (calls, results) = pending_durable_counts(snapshot, call_id);
    if (calls, results) != (1, 0) {
        return Err(
            format!("unexpected durable pending cut: calls={calls}, results={results}").into(),
        );
    }
    Ok(())
}

/// Counts the exact durable provider call and result at the pending cut.
fn pending_durable_counts(snapshot: &DurableSnapshot, call_id: &ToolCallId) -> (usize, usize) {
    let calls = snapshot
        .agent_events
        .iter()
        .filter(|record| {
            matches!(&record.event, Event::ProviderResponseFinished(finished)
            if finished.output_items.iter().any(|item| {
                matches!(item, tau_proto::ContextItem::ToolCall(call)
                    if &call.call_id == call_id)
            }))
        })
        .count();
    let results = snapshot
        .agent_events
        .iter()
        .filter(|record| {
            matches!(&record.event, Event::ProviderToolResult(result)
                if &result.call_id == call_id)
        })
        .count();
    (calls, results)
}

/// Polls the bounded semantic snapshot until the expected durable cut appears.
///
/// On deadline this returns the last complete snapshot so the caller's exact
/// content assertion reports the missing or duplicate facts.
fn wait_for_durable_snapshot(
    fixture: &GateFixture,
    session_id: &tau_proto::SessionId,
    deadline: Instant,
    ready: impl Fn(&DurableSnapshot) -> bool,
) -> Result<DurableSnapshot, Box<dyn std::error::Error>> {
    let mut last_snapshot = None;
    let mut last_error = None;
    loop {
        match DurableSnapshot::load(&fixture.tau_state(), session_id) {
            Ok(snapshot) if ready(&snapshot) => return Ok(snapshot),
            Ok(snapshot) => last_snapshot = Some(snapshot),
            Err(error) => last_error = Some(error),
        }
        let now = Instant::now();
        if deadline <= now {
            if let Some(snapshot) = last_snapshot {
                return Ok(snapshot);
            }
            return Err(last_error.unwrap_or_else(|| "no durable snapshot was readable".into()));
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(10)),
        );
    }
}

/// Checks exact typed live facts, durable result sequence, extension readiness,
/// and the provider's two-action trace.
fn assert_round_facts(
    fixture: &GateFixture,
    observer: &SideObserver,
    session_id: &tau_proto::SessionId,
    agent_id: &tau_proto::AgentId,
    ids: &ScenarioIds,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    assert_exact_ready_set(&observer.events)?;
    assert_tool_admission(&observer.events, agent_id, &ids.call_id, false)?;
    let snapshot = wait_for_durable_snapshot(fixture, session_id, deadline, |snapshot| {
        assert_durable_tool(snapshot, &ids.prompt, &ids.call_id, &ids.completion).is_ok()
    })?;
    assert_durable_tool(&snapshot, &ids.prompt, &ids.call_id, &ids.completion)?;
    let completions = observer
        .events
        .iter()
        .filter(|observed| provider_finished_contains(&observed.event, &ids.completion))
        .count();
    let results = observer
        .events
        .iter()
        .filter(|observed| {
            matches!(&observed.event, Event::ToolResultDisplay(result)
                if result.call_id == ids.call_id)
        })
        .count();
    assert_eq!((results, completions), (1, 1));
    let trace = fixture.trace()?;
    assert_eq!(
        trace
            .lines()
            .filter(|line| line.contains(" matched "))
            .count(),
        2
    );
    Ok(())
}

/// Shuts down both public terminals, proves daemon cleanup, and releases the
/// fixture only after every assertion has passed.
fn finish_pair(
    fixture: GateFixture,
    observer: SideObserver,
    attached: PtyProcess,
    original: PtyProcess,
    session_id: &tau_proto::SessionId,
) -> Result<(), Box<dyn std::error::Error>> {
    drop(observer);
    attached.finish()?;
    original.finish()?;
    fixture.require_boot_gone(session_id.as_str())?;
    fixture.complete();
    Ok(())
}
