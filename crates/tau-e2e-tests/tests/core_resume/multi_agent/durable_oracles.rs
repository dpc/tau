//! Typed durable-store and exact fake-provider trace oracles for S8.

use std::collections::BTreeSet;

use tau_e2e_tests::DurableSessionSnapshot;
use tau_proto::{ContextItem, Event, EventName, ToolCallId};

use super::{Identities, agent_start_projection};
use crate::GateFixture;

/// Validates Boot A's exact typed membership, restore, and agent record counts.
pub(super) fn assert_snapshot_a(
    snapshot: &DurableSessionSnapshot,
    identities: &Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let expected = identities
        .all()
        .into_iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if snapshot
        .agent_events
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>()
        != expected
        || snapshot.session_events.len() != 2
        || snapshot.restore_events.len() != 2
        || snapshot.agent_events[&identities.main].len() != 20
        || snapshot.agent_events[&identities.worker].len() != 6
    {
        return Err(format!(
            "S8 Boot A durable record counts changed: session={}, restore={}, main={}, worker={}",
            snapshot.session_events.len(),
            snapshot.restore_events.len(),
            snapshot.agent_events[&identities.main].len(),
            snapshot.agent_events[&identities.worker].len()
        )
        .into());
    }
    for agent_id in identities.all() {
        let events = &snapshot.agent_events[agent_id];
        if events.first().is_none_or(|record| {
            record.seq.get() != 0
                || !matches!(
                    &record.event,
                    Event::AgentStarted(started) if &started.agent_id == agent_id
                )
        }) {
            return Err(format!("S8 durable creation prefix changed for {agent_id}").into());
        }
        if events
            .iter()
            .enumerate()
            .any(|(index, record)| record.seq.get() != index as u64)
        {
            return Err(format!("S8 durable sequence is not contiguous for {agent_id}").into());
        }
    }
    assert_exact_event_names(snapshot, identities)?;
    for (record, agent_id) in snapshot.session_events.iter().zip(identities.all()) {
        if !matches!(
            &record.event,
            Event::SessionAgentLoaded(loaded)
                if loaded.session_id == snapshot.session_id
                    && &loaded.agent_id == agent_id
                    && !loaded.ephemeral
        ) {
            return Err(format!("S8 durable membership record changed: {record:?}").into());
        }
    }
    let [request, started] = snapshot.restore_events.as_slice() else {
        unreachable!("record count checked above")
    };
    let call_id = ToolCallId::from("s8-agent-start");
    if !matches!(
        &request.event,
        Event::ToolRequest(value)
            if value.call_id == call_id
                && value.agent_id == identities.main
                && value.tool_name.as_str() == "agent_start"
                && agent_start_projection::arguments_match(&value.arguments)
    ) || !matches!(
        &started.event,
        Event::ToolStarted(value)
            if value.call_id == call_id
                && value.agent_id == identities.main
                && value.tool_name.as_str() == "agent_start"
                && agent_start_projection::arguments_match(&value.arguments)
    ) {
        return Err(format!(
            "S8 durable execution-restore records changed: {:?}",
            snapshot.restore_events
        )
        .into());
    }
    assert_durable_agent_start(snapshot, identities)?;
    assert_boot_a_agent_payloads(snapshot, identities)?;
    Ok(())
}

fn assert_exact_event_names(
    snapshot: &DurableSessionSnapshot,
    identities: &Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    use EventName as E;
    let main_expected = [
        E::AGENT_STARTED,
        E::AGENT_USER_INTERACTION_RECORDED,
        E::AGENT_PROMPT_SUBMITTED,
        E::AGENT_INFERENCE_DISPATCH_STARTED,
        E::AGENT_PROMPT_STARTED,
        E::PROVIDER_RESPONSE_FINISHED,
        E::AGENT_MESSAGE_RECEIVED,
        E::PROVIDER_TOOL_RESULT,
        E::AGENT_INFERENCE_DISPATCH_STARTED,
        E::AGENT_PROMPT_STARTED,
        E::AGENT_MESSAGE_RECEIVED,
        E::PROVIDER_RESPONSE_FINISHED,
        E::AGENT_INFERENCE_DISPATCH_STARTED,
        E::AGENT_PROMPT_STARTED,
        E::AGENT_MESSAGE_RECEIVED,
        E::AGENT_MESSAGE_RECEIVED,
        E::PROVIDER_RESPONSE_FINISHED,
        E::AGENT_INFERENCE_DISPATCH_STARTED,
        E::AGENT_PROMPT_STARTED,
        E::PROVIDER_RESPONSE_FINISHED,
    ];
    let worker_expected = [
        E::AGENT_STARTED,
        E::AGENT_DISPLAY_NAME_SET,
        E::AGENT_PROMPT_SUBMITTED,
        E::AGENT_INFERENCE_DISPATCH_STARTED,
        E::AGENT_PROMPT_STARTED,
        E::PROVIDER_RESPONSE_FINISHED,
    ];
    for (agent_id, expected) in [
        (&identities.main, main_expected.as_slice()),
        (&identities.worker, worker_expected.as_slice()),
    ] {
        let actual = snapshot.agent_events[agent_id]
            .iter()
            .map(|record| record.event.name())
            .collect::<Vec<_>>();
        if actual != expected {
            return Err(
                format!("S8 durable event projection changed for {agent_id}: {actual:?}").into(),
            );
        }
    }
    Ok(())
}

fn assert_boot_a_agent_payloads(
    snapshot: &DurableSessionSnapshot,
    identities: &Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let main = &snapshot.agent_events[&identities.main];
    let worker = &snapshot.agent_events[&identities.worker];
    if !matches!(
        &main[0].event,
        Event::AgentStarted(started)
            if started.agent_id == identities.main
                && started.parent_agent.is_none()
                && started.role == "deterministic-main"
                && started.display_name.is_none()
                && started.metadata.is_empty()
                && !started.ephemeral
    ) || !matches!(
        &main[1].event,
        Event::AgentUserInteractionRecorded(value) if value.agent_id == identities.main
    ) || !matches!(
        &worker[0].event,
        Event::AgentStarted(started)
            if started.agent_id == identities.worker
                && started.parent_agent.as_ref() == Some(&identities.main)
                && started.role == "deterministic-worker"
                && started.display_name.as_deref() == Some("deterministic worker")
                && started.metadata.is_empty()
                && !started.ephemeral
    ) || !matches!(
        &worker[1].event,
        Event::AgentDisplayNameSet(name)
            if name.agent_id == identities.worker
                && name.display_name == "deterministic worker"
    ) {
        return Err("S8 exact durable creation payloads changed".into());
    }
    let main_prompts = main
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentPromptSubmitted(prompt) => Some(prompt),
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected_main_prompts = [super::MAIN_PROMPT.to_owned()];
    if main_prompts.len() != expected_main_prompts.len()
        || main_prompts
            .iter()
            .zip(&expected_main_prompts)
            .enumerate()
            .any(|(index, (prompt, text))| {
                prompt.agent_id != identities.main
                    || !prompt.inference_activation
                    || &prompt.text != text
                    || prompt.internal_kind.is_some()
                    || prompt.originator != tau_proto::PromptOriginator::User
                    || prompt.display_name.as_deref() != Some("main")
                    || if index == 0 {
                        prompt.message_class != tau_proto::PromptMessageClass::User
                            || prompt.submission_source
                                != tau_proto::PromptSubmissionSource::HumanUi
                            || prompt.ctx_id.as_deref() != Some("s8-main")
                    } else {
                        prompt.message_class != tau_proto::PromptMessageClass::Internal
                            || prompt.submission_source
                                != tau_proto::PromptSubmissionSource::HarnessInternal
                            || prompt.ctx_id.is_some()
                    }
            })
        || !matches!(
            &worker[2].event,
            Event::AgentPromptSubmitted(prompt)
                if prompt.agent_id == identities.worker
                    && prompt.inference_activation
                    && prompt.text == super::WORKER_INITIAL
                    && prompt.message_class == tau_proto::PromptMessageClass::User
                    && prompt.internal_kind.is_none()
                    && matches!(
                        &prompt.originator,
                        tau_proto::PromptOriginator::Extension { name, query_id }
                            if name.as_str() == "__harness__" && query_id == "delegate-0"
                    )
                    && prompt.submission_source
                        == tau_proto::PromptSubmissionSource::HarnessInternal
                    && prompt.display_name.as_deref() == Some("deterministic worker")
                    && prompt.ctx_id.is_none()
        )
        || !exact_text_response(
            &worker[5].event,
            &identities.worker,
            "worker boot-a complete",
            false,
        )
    {
        return Err("S8 exact durable prompt/worker payloads changed".into());
    }
    let messages = main
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentMessageReceived(message) => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected_messages = [
        (
            tau_proto::AgentMessageKind::WatchTurnState,
            format!(
                "[tau-internal]: Watched agent {} is not currently running an agent turn (initial watch state)",
                identities.worker
            ),
        ),
        (
            tau_proto::AgentMessageKind::WatchTurnState,
            format!(
                "[tau-internal]: Watched agent {} started an agent turn",
                identities.worker
            ),
        ),
        (
            tau_proto::AgentMessageKind::WatchResponse,
            "worker boot-a complete".to_owned(),
        ),
        (
            tau_proto::AgentMessageKind::WatchTurnState,
            format!(
                "[tau-internal]: Watched agent {} stopped its agent turn",
                identities.worker
            ),
        ),
    ];
    if messages.len() != expected_messages.len()
        || messages
            .iter()
            .zip(&expected_messages)
            .any(|(message, (kind, text))| {
                message.sender_id != identities.worker
                    || message.message_id.as_str().is_empty()
                    || message.sender_session_id.is_some()
                    || message.recipient_id != identities.main
                    || &message.kind != kind
                    || &message.message != text
                    || message.watch_provider_status.is_some()
            })
    {
        return Err("S8 exact durable watch-message payloads changed".into());
    }
    let states = messages
        .iter()
        .filter_map(|message| message.watch_turn_state.as_ref())
        .collect::<Vec<_>>();
    let [initial, running, idle] = states.as_slice() else {
        return Err("S8 exact durable watch state count changed".into());
    };
    if initial.session_id != snapshot.session_id
        || initial.subscription_id.is_empty()
        || !initial.initial
        || initial.state != tau_proto::AgentRuntimeState::Idle
        || initial.turn_generation != 0
        || running.session_id != snapshot.session_id
        || running.initial
        || running.state != tau_proto::AgentRuntimeState::Running
        || idle.session_id != snapshot.session_id
        || idle.initial
        || idle.state != tau_proto::AgentRuntimeState::Idle
        || running.subscription_id != initial.subscription_id
        || idle.subscription_id != initial.subscription_id
        || running.turn_generation == 0
        || idle.turn_generation != running.turn_generation
        || messages[2].watch_turn_state.is_some()
    {
        return Err("S8 exact durable structured watch state changed".into());
    }
    let text_responses = main
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderResponseFinished(finished)
                if matches!(finished.output_items.as_slice(), [ContextItem::Message(_)]) =>
            {
                Some(&record.event)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected = [
        "worker start accepted",
        "watch notification accepted",
        "worker completion observed",
    ];
    if text_responses.len() != expected.len()
        || text_responses
            .iter()
            .zip(expected)
            .any(|(event, marker)| !exact_text_response(event, &identities.main, marker, true))
    {
        return Err("S8 exact durable main terminal payloads changed".into());
    }
    assert_inference_rounds(
        main,
        &identities.main,
        &[0, 3, 5, 8],
        &[
            tau_proto::AgentHead::Root,
            tau_proto::AgentHead::Node(tau_proto::NodeId::new(2)),
            tau_proto::AgentHead::Node(tau_proto::NodeId::new(3)),
            tau_proto::AgentHead::Node(tau_proto::NodeId::new(5)),
        ],
    )?;
    assert_inference_rounds(
        worker,
        &identities.worker,
        &[0],
        &[tau_proto::AgentHead::Root],
    )?;
    Ok(())
}

fn assert_inference_rounds(
    records: &[tau_core::PersistedAgentEvent],
    agent_id: &tau_proto::AgentId,
    through_nodes: &[u64],
    activation_cuts: &[tau_proto::AgentHead],
) -> Result<(), Box<dyn std::error::Error>> {
    let dispatches = records.iter().filter_map(|record| match &record.event {
        Event::AgentInferenceDispatchStarted(dispatch) => Some(dispatch),
        _ => None,
    });
    let prompts = records.iter().filter_map(|record| match &record.event {
        Event::AgentPromptStarted(prompt) => Some(prompt),
        _ => None,
    });
    let responses = records.iter().filter_map(|record| match &record.event {
        Event::ProviderResponseFinished(response) => Some(response),
        _ => None,
    });
    let rounds = dispatches.zip(prompts).zip(responses).collect::<Vec<_>>();
    if rounds.len() != through_nodes.len()
        || rounds.len() != activation_cuts.len()
        || rounds
            .iter()
            .zip(through_nodes.iter().zip(activation_cuts))
            .enumerate()
            .any(
                |(index, (((dispatch, prompt), response), (through, cut)))| {
                    &dispatch.agent_id != agent_id
                        || &prompt.agent_id != agent_id
                        || &response.agent_id != agent_id
                        || dispatch.agent_prompt_id != prompt.agent_prompt_id
                        || dispatch.agent_prompt_id != response.agent_prompt_id
                        || dispatch.agent_prompt_id.as_str()
                            != format!("ap-{}-{index}", agent_id.as_str())
                        || dispatch.transaction_id.is_some()
                        || dispatch.through
                            != tau_proto::AgentHead::Node(tau_proto::NodeId::new(*through))
                        || dispatch.model.as_ref().map(ToString::to_string).as_deref()
                            != Some("fake/test")
                        || dispatch.operation != Some(tau_proto::PromptOperation::Inference)
                        || dispatch.activation_cut.as_ref() != Some(cut)
                        || prompt.model.to_string() != "fake/test"
                        || prompt.operation != tau_proto::PromptOperation::Inference
                        || !terminal_defaults(response)
                },
            )
    {
        return Err(format!("S8 inference round projection changed for {agent_id}").into());
    }
    Ok(())
}

fn exact_text_response(
    event: &Event,
    agent_id: &tau_proto::AgentId,
    text: &str,
    user_originator: bool,
) -> bool {
    matches!(
        event,
        Event::ProviderResponseFinished(finished)
            if &finished.agent_id == agent_id
                && finished.stop_reason == tau_proto::ProviderStopReason::EndTurn
                && terminal_defaults(finished)
                && if user_originator {
                    finished.originator == tau_proto::PromptOriginator::User
                } else {
                    matches!(
                        &finished.originator,
                        tau_proto::PromptOriginator::Extension { name, query_id }
                            if name.as_str() == "__harness__" && query_id == "delegate-0"
                    )
                }
                && matches!(
                    finished.output_items.as_slice(),
                    [ContextItem::Message(message)]
                        if message.role == tau_proto::ContextRole::Assistant
                            && matches!(
                                message.content.as_slice(),
                                [tau_proto::ContentPart::Text { text: actual }] if actual == text
                            )
                )
    )
}

fn terminal_defaults(finished: &tau_proto::ProviderResponseFinished) -> bool {
    finished.error.is_none()
        && finished.failure_kind.is_none()
        && finished.context_limit_telemetry.is_none()
        && finished.recovery_disposition == tau_proto::ContextRecoveryDisposition::None
        && finished.compaction_original_input_tokens.is_none()
        && finished.compaction_compacted_input_tokens.is_none()
        && finished.backend.is_none()
        && finished.provider_response_id.is_none()
        && finished.ws_pool_delta.is_none()
        && finished.usage.is_some()
}

fn assert_durable_agent_start(
    snapshot: &DurableSessionSnapshot,
    identities: &Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let call_id = ToolCallId::from("s8-agent-start");
    let main = &snapshot.agent_events[&identities.main];
    let calls = main
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            matches!(
                &record.event,
                Event::ProviderResponseFinished(finished)
                    if finished.agent_id == identities.main
                        && finished.stop_reason == tau_proto::ProviderStopReason::ToolCalls
                        && finished.originator == tau_proto::PromptOriginator::User
                        && terminal_defaults(finished)
                        && matches!(
                            finished.output_items.as_slice(),
                            [ContextItem::ToolCall(call)]
                                if call.call_id == call_id
                                    && call.name.as_str() == "agent_start"
                                    && agent_start_projection::arguments_match(&call.arguments)
                        )
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let results = main
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            matches!(
                &record.event,
                Event::ProviderToolResult(result)
                    if result.call_id == call_id
                        && result.tool_name.as_str() == "agent_start"
                        && result.tool_type == tau_proto::ToolType::Function
                        && result.kind == tau_proto::ToolResultKind::Final
                        && result.provider_content.is_empty()
                        && result.originator == tau_proto::PromptOriginator::User
                        && agent_start_projection::result_ids_match(
                            &result.result,
                            &identities.main,
                            &identities.worker,
                        )
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let ([call], [result]) = (calls.as_slice(), results.as_slice()) else {
        return Err(format!(
            "S8 durable agent_start call/result counts changed: call={calls:?}, result={results:?}"
        )
        .into());
    };
    if call >= result {
        return Err("S8 durable agent_start result preceded its call".into());
    }
    Ok(())
}

/// Validates Boot B's immutable prefixes and exact targeted-worker suffix.
pub(super) fn assert_snapshot_suffix(
    before: &DurableSessionSnapshot,
    after: &DurableSessionSnapshot,
    identities: &Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    if after.session_events != before.session_events
        || after.restore_events != before.restore_events
        || after.agent_events[&identities.main] != before.agent_events[&identities.main]
    {
        return Err(
            "S8 resume changed membership, restore state, or the untargeted main journal".into(),
        );
    }
    let worker_before = &before.agent_events[&identities.worker];
    let worker_after = &after.agent_events[&identities.worker];
    let suffix = &worker_after[worker_before.len()..];
    let [
        interaction,
        notice,
        prompt,
        dispatch,
        started,
        response_record,
    ] = suffix
    else {
        return Err(format!(
            "S8 worker durable suffix has {} records instead of six",
            suffix.len()
        )
        .into());
    };
    if !matches!(
        &interaction.event,
        Event::AgentUserInteractionRecorded(value) if value.agent_id == identities.worker
    ) || !matches!(
        &notice.event,
        Event::AgentPromptSubmitted(value)
            if value.agent_id == identities.worker
                && !value.inference_activation
                && value.message_class == tau_proto::PromptMessageClass::Internal
                && value.submission_source == tau_proto::PromptSubmissionSource::HarnessInternal
                && value.text == super::RESTORE_NOTICE
                && value.internal_kind.is_none()
                && value.originator == tau_proto::PromptOriginator::User
                && value.display_name.as_deref() == Some("deterministic worker")
                && value.ctx_id.is_none()
    ) || !matches!(
        &prompt.event,
        Event::AgentPromptSubmitted(value)
            if value.agent_id == identities.worker
                && value.inference_activation
                && value.text == "fresh worker work"
                && value.message_class == tau_proto::PromptMessageClass::User
                && value.submission_source == tau_proto::PromptSubmissionSource::HumanUi
                && value.internal_kind.is_none()
                && value.originator == tau_proto::PromptOriginator::User
                && value.display_name.as_deref() == Some("deterministic worker")
                && value.ctx_id.is_none()
    ) {
        return Err(format!("S8 worker durable pre-dispatch suffix changed: {suffix:?}").into());
    }
    let Event::AgentInferenceDispatchStarted(dispatch) = &dispatch.event else {
        return Err("S8 worker durable suffix omitted its dispatch checkpoint".into());
    };
    let Event::AgentPromptStarted(started) = &started.event else {
        return Err("S8 worker durable suffix omitted its prompt-start checkpoint".into());
    };
    let Event::ProviderResponseFinished(response) = &response_record.event else {
        return Err("S8 worker durable suffix omitted its terminal response".into());
    };
    if dispatch.agent_id != identities.worker
        || started.agent_id != identities.worker
        || dispatch.agent_prompt_id != response.agent_prompt_id
        || dispatch.agent_prompt_id != started.agent_prompt_id
        || dispatch.agent_prompt_id.as_str() != format!("ap-{}-1", identities.worker.as_str())
        || dispatch.transaction_id.is_some()
        || dispatch.through != tau_proto::AgentHead::Node(tau_proto::NodeId::new(3))
        || dispatch.model.as_ref().map(ToString::to_string).as_deref() != Some("fake/test")
        || dispatch.operation != Some(tau_proto::PromptOperation::Inference)
        || dispatch.activation_cut != Some(tau_proto::AgentHead::Node(tau_proto::NodeId::new(2)))
        || started.model.to_string() != "fake/test"
        || started.operation != tau_proto::PromptOperation::Inference
        || response.agent_id != identities.worker
        || !exact_text_response(
            &response_record.event,
            &identities.worker,
            "fresh worker complete",
            true,
        )
    {
        return Err("S8 worker durable dispatch/prompt/terminal correlation changed".into());
    }
    let prompt_count = suffix
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentPromptSubmitted(prompt)
                    if prompt.agent_id == identities.worker
                        && prompt.text == "fresh worker work"
            )
        })
        .count();
    let response_count = suffix
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::ProviderResponseFinished(finished)
                    if finished.agent_id == identities.worker
                        && finished.output_items.iter().any(|item| {
                            matches!(
                                item,
                                ContextItem::Message(message)
                                    if message.content.iter().any(|part| {
                                        matches!(
                                            part,
                                            tau_proto::ContentPart::Text { text }
                                                if text == "fresh worker complete"
                                        )
                                    })
                            )
                        })
            )
        })
        .count();
    if prompt_count != 1 || response_count != 1 {
        return Err(format!(
            "S8 worker durable suffix changed: records={}, prompt={prompt_count}, response={response_count}",
            suffix.len()
        )
        .into());
    }
    Ok(())
}

/// Counts fully matched closed scenario actions in the fake-provider trace.
pub(super) fn matched_actions(fixture: &GateFixture) -> Result<usize, Box<dyn std::error::Error>> {
    Ok(fixture
        .trace()?
        .lines()
        .filter(|line| line.contains(" matched "))
        .count())
}

/// Requires all five S8 actions and both lanes to be exactly exhausted.
pub(super) fn assert_exact_consumption(
    fixture: &GateFixture,
) -> Result<(), Box<dyn std::error::Error>> {
    let trace = fixture.trace()?;
    let matched = trace
        .lines()
        .filter(|line| line.contains(" matched "))
        .count();
    let exhausted = trace
        .lines()
        .filter(|line| line.ends_with("remaining=0"))
        .count();
    if matched != 5 || exhausted != 2 || trace.contains("mismatch") {
        return Err(format!(
            "S8 scenario consumption changed: matched={matched}, exhausted={exhausted}"
        )
        .into());
    }
    Ok(())
}
