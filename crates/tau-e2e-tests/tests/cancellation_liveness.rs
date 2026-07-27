use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tau_core::{AgentStore, SessionStore};
use tau_e2e_tests::{DeterministicFixture, ScenarioActionV2, ScenarioLaneV2, ScenarioV2};
use tau_proto::{Event, ProviderStopReason};
use tau_socket::SocketPeer;

#[path = "deterministic_provider/daemon_support.rs"]
mod daemon_support;

use daemon_support::*;

const FAKE_PROVIDER: &str = env!("CARGO_BIN_EXE_tau-e2e-fake-provider");
const HARNESS_DAEMON: &str = env!("CARGO_BIN_EXE_tau-e2e-harness-daemon");

/// Proves exact cancellation isolates two held provider prompts, produces one
/// terminal per target without a late response, and leaves the second selected
/// agent's bound lane able to complete fresh work before bounded shutdown.
#[test]
fn exact_cancellation_isolates_lanes_and_preserves_same_agent_liveness()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = DeterministicFixture::new_v2(
        "exact_cancellation_isolates_lanes_and_preserves_same_agent_liveness",
        &ScenarioV2::new(
            "gate-3-cancel-and-reuse",
            vec![
                ScenarioLaneV2 {
                    ctx_id: "cancel-a".to_owned(),
                    actions: vec![ScenarioActionV2::HoldUntilCancel {
                        user_text: "hold lane a".to_owned(),
                        timeout_ms: 5_000,
                    }],
                },
                ScenarioLaneV2 {
                    ctx_id: "cancel-b".to_owned(),
                    actions: vec![
                        ScenarioActionV2::HoldUntilCancel {
                            user_text: "hold lane b".to_owned(),
                            timeout_ms: 5_000,
                        },
                        ScenarioActionV2::Text {
                            user_text: "continue lane b".to_owned(),
                            response: "lane b remains live".to_owned(),
                        },
                    ],
                },
            ],
        ),
        FAKE_PROVIDER,
    )?;
    let socket = fixture.socket_path("gate-3");
    let server = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
    let mut peer = connect_ui(&socket)?;
    let mut observed = ObservedLifecycle::default();

    create_agent(&mut peer, "cancel-a", "hold lane a")?;
    let held_a = wait_for_dispatched_prompt(&mut peer, &mut observed, "cancel-a")?;
    create_agent(&mut peer, "cancel-b", "hold lane b")?;
    let held_b = wait_for_dispatched_prompt(&mut peer, &mut observed, "cancel-b")?;
    if held_a.agent_id == held_b.agent_id || held_a.agent_prompt_id == held_b.agent_prompt_id {
        return Err("independent cancellation lanes reused an agent or prompt identity".into());
    }

    cancel_prompt(&mut peer, &held_a)?;
    wait_for_exact_cancellation(&mut peer, &mut observed, &held_a, &[&held_a, &held_b])?;
    observed.require_held(&held_b)?;

    cancel_prompt(&mut peer, &held_b)?;
    wait_for_exact_cancellation(&mut peer, &mut observed, &held_b, &[&held_b])?;

    submit_prompt(&mut peer, &held_b.agent_id, "cancel-b", "continue lane b")?;
    let fresh = wait_for_dispatched_prompt(&mut peer, &mut observed, "cancel-b")?;
    if fresh.agent_id != held_b.agent_id || fresh.agent_prompt_id == held_b.agent_prompt_id {
        return Err(
            "post-cancel work did not reuse the surviving agent with a fresh prompt".into(),
        );
    }
    wait_for_success(&mut peer, &mut observed, &fresh, "lane b remains live")?;
    observed.require_outcome(&held_a, ExpectedOutcome::Canceled)?;
    observed.require_outcome(&held_b, ExpectedOutcome::Canceled)?;
    observed.require_outcome(&fresh, ExpectedOutcome::Finished)?;
    observed.require_exact_totals()?;

    disconnect_ui(&mut peer)?;
    server.finish()?;
    assert_durable_lifecycle(&fixture, &held_a, &held_b, &fresh)?;

    let trace = fixture.trace()?;
    assert_eq!(trace.matches("hold_canceled").count(), 2);
    assert!(!trace.contains("hold_timeout"));
    assert!(!trace.contains("cancel_after_timeout"));
    fixture.assert_consumed()?;
    Ok(())
}

/// Typed socket lifecycle facts collected for the three exact prompt ids.
#[derive(Default)]
struct ObservedLifecycle {
    /// Exact typed prompt-creation count keyed by prompt identity.
    created: BTreeMap<tau_proto::AgentPromptId, usize>,
    /// Exact provider-dispatch count keyed by prompt identity.
    submitted: BTreeMap<tau_proto::AgentPromptId, usize>,
    /// Exact harness cancellation-terminal count keyed by prompt identity.
    terminated: BTreeMap<tau_proto::AgentPromptId, usize>,
    /// Exact accepted provider terminal count keyed by prompt identity.
    finished: BTreeMap<tau_proto::AgentPromptId, usize>,
    /// Exact provider cancellation acknowledgement count keyed by prompt
    /// identity.
    acknowledged: BTreeMap<tau_proto::AgentPromptId, usize>,
    /// Sorted prompt identities the provider reported active at cancellation.
    active_before: BTreeMap<tau_proto::AgentPromptId, Vec<tau_proto::AgentPromptId>>,
}

/// Expected terminal state for one exactly correlated prompt.
#[derive(Clone, Copy)]
enum ExpectedOutcome {
    /// Prompt remains held without a terminal or acknowledgement.
    Held,
    /// Harness emitted one cancellation terminal and provider acknowledged it.
    Canceled,
    /// Harness accepted one successful provider terminal.
    Finished,
}

/// Typed provider acknowledgement embedded in the bounded trace notice.
#[derive(Deserialize)]
struct CancelAcknowledgement {
    /// Prompt selected by the cancellation request.
    selected: tau_proto::AgentPromptId,
    /// Exact prompt identity that woke the bounded hold worker.
    canceled_by: tau_proto::AgentPromptId,
    /// Sorted hold identities present before the provider removed the target.
    active_before: Vec<tau_proto::AgentPromptId>,
}

impl ObservedLifecycle {
    fn record(&mut self, event: &Event) -> Result<(), Box<dyn std::error::Error>> {
        match event {
            Event::AgentPromptCreated(value) => {
                *self
                    .created
                    .entry(value.agent_prompt_id.clone())
                    .or_default() += 1;
            }
            Event::ProviderPromptSubmitted(value) => {
                *self
                    .submitted
                    .entry(value.agent_prompt_id.clone())
                    .or_default() += 1;
            }
            Event::AgentPromptTerminated(value) => {
                if value.reason != tau_proto::AgentPromptTerminationReason::Canceled {
                    return Err(format!(
                        "prompt {} terminated as {:?}, not canceled",
                        value.agent_prompt_id, value.reason
                    )
                    .into());
                }
                *self
                    .terminated
                    .entry(value.agent_prompt_id.clone())
                    .or_default() += 1;
            }
            Event::ProviderResponseFinished(value) => {
                *self
                    .finished
                    .entry(value.agent_prompt_id.clone())
                    .or_default() += 1;
            }
            Event::HarnessNotice(notice) if notice.level == tau_proto::NoticeLevel::Trace => {
                let Some(payload) = notice
                    .message
                    .strip_prefix("e2e_fake_provider.cancel_completed ")
                else {
                    return Ok(());
                };
                let payload: CancelAcknowledgement = serde_json::from_str(payload)?;
                if payload.canceled_by != payload.selected {
                    return Err("provider cancellation acknowledgement changed identity".into());
                }
                self.active_before
                    .insert(payload.selected.clone(), payload.active_before);
                *self.acknowledged.entry(payload.selected).or_default() += 1;
            }
            _ => {}
        }
        Ok(())
    }

    fn require_exact_totals(&self) -> Result<(), Box<dyn std::error::Error>> {
        let totals = (
            self.created.len(),
            self.submitted.len(),
            self.terminated.len(),
            self.finished.len(),
            self.acknowledged.len(),
        );
        if totals != (3, 3, 2, 1, 2) {
            return Err(format!(
                "unexpected lifecycle identity totals \
                 created/submitted/terminated/finished/acknowledged={totals:?}"
            )
            .into());
        }
        Ok(())
    }

    fn require_held(
        &self,
        prompt: &tau_proto::AgentPromptCreated,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.require_outcome(prompt, ExpectedOutcome::Held)
    }

    fn require_outcome(
        &self,
        prompt: &tau_proto::AgentPromptCreated,
        expected: ExpectedOutcome,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (terminated, finished, acknowledged) = match expected {
            ExpectedOutcome::Held => (0, 0, 0),
            ExpectedOutcome::Canceled => (1, 0, 1),
            ExpectedOutcome::Finished => (0, 1, 0),
        };
        let id = &prompt.agent_prompt_id;
        let counts = (
            self.created.get(id).copied().unwrap_or_default(),
            self.submitted.get(id).copied().unwrap_or_default(),
            self.terminated.get(id).copied().unwrap_or_default(),
            self.finished.get(id).copied().unwrap_or_default(),
            self.acknowledged.get(id).copied().unwrap_or_default(),
        );
        if counts != (1, 1, terminated, finished, acknowledged) {
            return Err(format!(
                "unexpected lifecycle counts for {id}: \
                 created/submitted/terminated/finished/acknowledged={counts:?}"
            )
            .into());
        }
        Ok(())
    }
}

fn observe(
    peer: &mut SocketPeer,
    lifecycle: &mut ObservedLifecycle,
) -> Result<Event, Box<dyn std::error::Error>> {
    loop {
        let observed = recv_observed(peer)?;
        if observed.replay {
            if matches!(
                observed.event,
                Event::AgentPromptCreated(_)
                    | Event::ProviderPromptSubmitted(_)
                    | Event::AgentPromptTerminated(_)
                    | Event::ProviderResponseFinished(_)
            ) {
                return Err("cancellation lifecycle was delivered as historical replay".into());
            }
            continue;
        }
        lifecycle.record(&observed.event)?;
        return Ok(observed.event);
    }
}

fn wait_for_dispatched_prompt(
    peer: &mut SocketPeer,
    lifecycle: &mut ObservedLifecycle,
    ctx_id: &str,
) -> Result<tau_proto::AgentPromptCreated, Box<dyn std::error::Error>> {
    let mut created = None;
    loop {
        match observe(peer, lifecycle)? {
            Event::AgentPromptCreated(value) if value.ctx_id.as_deref() == Some(ctx_id) => {
                created = Some(value);
            }
            Event::ProviderPromptSubmitted(value)
                if created
                    .as_ref()
                    .is_some_and(|created| value.agent_prompt_id == created.agent_prompt_id) =>
            {
                return Ok(created.expect("matching created prompt is present"));
            }
            _ => {}
        }
    }
}

fn wait_for_exact_cancellation(
    peer: &mut SocketPeer,
    lifecycle: &mut ObservedLifecycle,
    selected: &tau_proto::AgentPromptCreated,
    active_before: &[&tau_proto::AgentPromptCreated],
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        observe(peer, lifecycle)?;
        for other in active_before {
            if other.agent_prompt_id != selected.agent_prompt_id {
                lifecycle.require_held(other)?;
            }
        }
        if lifecycle.terminated.get(&selected.agent_prompt_id).copied() == Some(1)
            && lifecycle
                .acknowledged
                .get(&selected.agent_prompt_id)
                .copied()
                == Some(1)
        {
            let mut expected_active = active_before
                .iter()
                .map(|prompt| prompt.agent_prompt_id.clone())
                .collect::<Vec<_>>();
            expected_active.sort();
            if lifecycle.active_before.get(&selected.agent_prompt_id) != Some(&expected_active) {
                return Err(format!(
                    "provider cancellation active set did not exactly correlate with {}",
                    selected.agent_prompt_id
                )
                .into());
            }
            lifecycle.require_outcome(selected, ExpectedOutcome::Canceled)?;
            return Ok(());
        }
    }
}

fn wait_for_success(
    peer: &mut SocketPeer,
    lifecycle: &mut ObservedLifecycle,
    selected: &tau_proto::AgentPromptCreated,
    marker: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        if let Event::ProviderResponseFinished(value) = observe(peer, lifecycle)?
            && value.agent_prompt_id == selected.agent_prompt_id
        {
            if value.agent_id != selected.agent_id
                || value.stop_reason != ProviderStopReason::EndTurn
                || value.error.is_some()
                || value.failure_kind.is_some()
                || !value.output_items.iter().any(|item| {
                    matches!(
                        item,
                        tau_proto::ContextItem::Message(message)
                            if message.content.iter().any(|part| matches!(
                                part,
                                tau_proto::ContentPart::Text { text } if text == marker
                            ))
                    )
                })
            {
                return Err(
                    "fresh same-agent prompt did not produce the exact successful terminal".into(),
                );
            }
            return Ok(());
        }
    }
}

fn assert_durable_lifecycle(
    fixture: &DeterministicFixture,
    held_a: &tau_proto::AgentPromptCreated,
    held_b: &tau_proto::AgentPromptCreated,
    fresh: &tau_proto::AgentPromptCreated,
) -> Result<(), Box<dyn std::error::Error>> {
    let session_id = tau_proto::SessionId::parse("deterministic-e2e-session")
        .expect("known-safe SessionId must be valid");
    let mut sessions = SessionStore::open(fixture.harness_state_dir().join("sessions"))?;
    let membership = sessions
        .load_session(session_id.as_str())?
        .ok_or("missing durable cancellation session")?;
    let loaded = membership.loaded_agents();
    if loaded.len() != 2
        || !loaded.iter().any(|agent_id| **agent_id == held_a.agent_id)
        || !loaded.iter().any(|agent_id| **agent_id == held_b.agent_id)
    {
        return Err(format!("unexpected durable cancellation membership: {loaded:?}").into());
    }
    let session_events = sessions.session_events(session_id.as_str())?;
    if session_events.len() != 2
        || session_events.iter().any(|record| {
            !matches!(
                &record.event,
                Event::SessionAgentLoaded(value)
                    if value.session_id == session_id && !value.ephemeral
            )
        })
    {
        return Err("durable session membership is not exactly two persistent loads".into());
    }

    let agents = AgentStore::open(fixture.harness_state_dir().join("agents"))?;
    let records_a = agents.agent_events(held_a.agent_id.as_str())?;
    let records_b = agents.agent_events(held_b.agent_id.as_str())?;
    assert_agent_records(&records_a, &[("hold lane a", held_a)], &[held_a], None)?;
    assert_agent_records(
        &records_b,
        &[("hold lane b", held_b), ("continue lane b", fresh)],
        &[held_b],
        Some((fresh, "lane b remains live")),
    )?;
    Ok(())
}

fn assert_agent_records(
    records: &[tau_core::PersistedAgentEvent],
    prompts: &[(&str, &tau_proto::AgentPromptCreated)],
    canceled: &[&tau_proto::AgentPromptCreated],
    successful: Option<(&tau_proto::AgentPromptCreated, &str)>,
) -> Result<(), Box<dyn std::error::Error>> {
    for (text, prompt) in prompts {
        let submitted = records
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::AgentPromptSubmitted(value)
                        if value.text == *text && value.agent_id == prompt.agent_id
                )
            })
            .count();
        let checkpointed = records
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::AgentInferenceDispatchStarted(value)
                        if value.agent_prompt_id == prompt.agent_prompt_id
                            && value.agent_id == prompt.agent_id
                            && value.operation == Some(tau_proto::PromptOperation::Inference)
                )
            })
            .count();
        if submitted != 1 || checkpointed != 1 {
            return Err(format!(
                "durable prompt {} is not unique: \
                 submitted={submitted}, checkpointed={checkpointed}",
                prompt.agent_prompt_id
            )
            .into());
        }
    }
    for prompt in canceled {
        if records.iter().any(|record| {
            matches!(
                &record.event,
                Event::ProviderResponseFinished(value)
                    if value.agent_prompt_id == prompt.agent_prompt_id
            )
        }) {
            return Err(format!(
                "canceled prompt {} gained a durable provider terminal",
                prompt.agent_prompt_id
            )
            .into());
        }
    }
    if let Some((prompt, marker)) = successful {
        let finishes = records
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::ProviderResponseFinished(value)
                        if value.agent_prompt_id == prompt.agent_prompt_id
                            && value.stop_reason == ProviderStopReason::EndTurn
                            && value.error.is_none()
                            && value.output_items.iter().any(|item| matches!(
                                item,
                                tau_proto::ContextItem::Message(message)
                                    if message.content.iter().any(|part| matches!(
                                        part,
                                        tau_proto::ContentPart::Text { text } if text == marker
                                    ))
                            ))
                )
            })
            .count();
        if finishes != 1 {
            return Err(format!(
                "fresh prompt {} has {finishes} durable successful terminals",
                prompt.agent_prompt_id
            )
            .into());
        }
    }
    Ok(())
}
