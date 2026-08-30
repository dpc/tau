use super::*;
use crate::internal_tools::{
    InternalToolDispatchWork, internal_tool_dispatch_work, reset_internal_tool_dispatch_work,
};

/// How one synthetic handler reacts after materializing a matching start.
#[derive(Clone, Copy)]
enum StartReaction {
    /// Retain only the visit.
    Record,
    /// Publish a nested non-start event through the production host.
    PublishNotice,
    /// Stop dispatch with a handler error.
    Error,
}

/// Work performed by the previous broadcast/materialize/filter loop.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct LegacyStartedDispatchWork {
    /// Handlers invoked before ownership was known.
    handler_invocations: usize,
    /// Deep argument clones made while materializing calls for those handlers.
    argument_clones: usize,
}

/// Execute the previous broadcast-first algorithm as a differential oracle.
///
/// Each handler first materializes the complete call, including a real deep
/// clone of `arguments`, and only then filters on the resolved internal name.
fn legacy_started_dispatch(
    started: &tau_proto::ToolStarted,
    correlation_present: bool,
) -> (Vec<&'static str>, LegacyStartedDispatchWork) {
    let mut outcomes = Vec::new();
    let mut work = LegacyStartedDispatchWork::default();
    for (label, owned_name) in [
        ("non-owner-a", "other-a"),
        ("owner-a", "target"),
        ("owner-b", "target"),
        ("non-owner-b", "other-b"),
    ] {
        work.handler_invocations += 1;
        if !correlation_present {
            continue;
        }
        let call = AgentToolCall {
            call_ref: None,
            id: started.call_id.clone(),
            name: ToolName::new("target"),
            tool_type: tau_proto::ToolType::Function,
            arguments: started.arguments.clone(),
        };
        work.argument_clones += 1;
        if call.name.as_str() == owned_name {
            outcomes.push(label);
        }
    }
    (outcomes, work)
}

/// Handler that exposes production dispatch order and materialization work.
struct RecordingInternalHandler {
    /// Stable label retained in visit order.
    label: &'static str,
    /// Internal name claimed by this handler.
    owned_name: &'static str,
    /// Reaction used for a correlated start.
    reaction: StartReaction,
    /// Shared ordered visit log.
    visits: path_std_sync::Arc<path_std_sync::Mutex<Vec<String>>>,
}

impl crate::InternalToolHandler for RecordingInternalHandler {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn handles(&self, internal_tool_name: &ToolName) -> bool {
        internal_tool_name.as_str() == self.owned_name
    }

    fn handle_event(
        &self,
        host: &mut crate::InternalToolHost<'_>,
        event: &Event,
    ) -> Result<(), HarnessError> {
        let event_name = match event {
            Event::ToolStarted(_) => "start",
            Event::ToolCancelRequest(_) => "cancel",
            Event::HarnessNotice(_) => "notice",
            _ => "other",
        };
        self.visits
            .lock()
            .expect("visit log")
            .push(format!("{}:{event_name}", self.label));
        let Event::ToolStarted(started) = event else {
            return Ok(());
        };
        let Some((_conversation_id, call, _visible_name)) = host.internal_started_call(started)
        else {
            return Ok(());
        };
        assert_eq!(call.name.as_str(), self.owned_name);
        match self.reaction {
            StartReaction::Record => Ok(()),
            StartReaction::PublishNotice => {
                host.emit_info_important("nested internal dispatch test");
                Ok(())
            }
            StartReaction::Error => Err(HarnessError::Participant(format!(
                "{} rejected the test call",
                self.label
            ))),
        }
    }
}

/// Construct one harness with two owners separated by handlers that would have
/// materialized every start under the previous broadcast-first dispatch.
fn internal_dispatch_harness(
    state: &std::path::Path,
    first_reaction: StartReaction,
) -> (
    Harness,
    AgentId,
    tau_proto::AgentId,
    path_std_sync::Arc<path_std_sync::Mutex<Vec<String>>>,
) {
    let mut harness = echo_harness(state).expect("start harness");
    let conversation_id = ensure_test_user_agent(&mut harness);
    let agent_id = harness
        .ensure_agent_id_for_agent(&conversation_id)
        .expect("test agent id");
    let agent_id = tau_proto::AgentId::parse(agent_id).expect("generated agent id");
    let visits = path_std_sync::Arc::new(path_std_sync::Mutex::new(Vec::new()));
    let handlers = [
        ("non-owner-a", "other-a", StartReaction::Record),
        ("owner-a", "target", first_reaction),
        ("owner-b", "target", StartReaction::Record),
        ("non-owner-b", "other-b", StartReaction::Record),
    ]
    .map(|(label, owned_name, reaction)| {
        path_std_sync::Arc::new(RecordingInternalHandler {
            label,
            owned_name,
            reaction,
            visits: visits.clone(),
        }) as path_std_sync::Arc<dyn crate::InternalToolHandler>
    });
    harness.tool_routing.internal_tool_handlers = handlers.into();
    (harness, conversation_id, agent_id, visits)
}

/// Correlate one synthetic start through either model-agent or peer ownership.
fn correlated_start(
    harness: &mut Harness,
    conversation_id: &AgentId,
    agent_id: &tau_proto::AgentId,
    call_id: &str,
    payload_bytes: usize,
    peer_owned: bool,
) -> Event {
    let call_id = ToolCallId::from(call_id);
    let owners = if peer_owned {
        &mut harness.tool_routing.tool_runtime.peer_internal_tool_agents
    } else {
        &mut harness.tool_routing.tool_runtime.tool_agents
    };
    owners.insert(call_id.clone(), conversation_id.clone());
    harness.tool_routing.tool_runtime.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: ToolName::new("visible_target"),
            internal_name: ToolName::new("target"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    Event::ToolStarted(tau_proto::ToolStarted {
        call_id,
        tool_name: ToolName::new("visible_target"),
        arguments: CborValue::Bytes(vec![0x5a; payload_bytes]),
        agent_id: agent_id.clone(),
        originator: tau_proto::PromptOriginator::User,
        invocation_policy: tau_proto::ToolInvocationPolicy::default(),
    })
}

/// The selected-owner path must preserve owner order and peer/model correlation
/// while eliminating non-owner deep clones for 1–8 MiB payloads.
#[test]
fn started_internal_dispatch_selects_owners_before_large_argument_clone() {
    for (index, payload_bytes) in [1, 2, 4, 8]
        .into_iter()
        .map(|mib| mib * 1024 * 1024)
        .enumerate()
    {
        for peer_owned in [false, true] {
            let temp = TempDir::new().expect("tempdir");
            let (mut harness, conversation_id, agent_id, visits) =
                internal_dispatch_harness(temp.path(), StartReaction::Record);
            let event = correlated_start(
                &mut harness,
                &conversation_id,
                &agent_id,
                &format!("large-{index}-{peer_owned}"),
                payload_bytes,
                peer_owned,
            );

            reset_internal_tool_dispatch_work();
            harness
                .dispatch_internal_tool_event(&event)
                .expect("selected owners handle start");

            assert_eq!(
                internal_tool_dispatch_work(),
                InternalToolDispatchWork {
                    ownership_predicate_visits: 4,
                    handler_invocations: 2,
                    argument_clones: 2,
                }
            );
            let actual_outcomes = visits
                .lock()
                .expect("visit log")
                .iter()
                .map(|visit| {
                    visit
                        .strip_suffix(":start")
                        .expect("start visit")
                        .to_owned()
                })
                .collect::<Vec<_>>();
            let Event::ToolStarted(started) = &event else {
                unreachable!("fixture always builds a start");
            };
            let (legacy_outcomes, legacy_work) = legacy_started_dispatch(started, true);
            assert_eq!(actual_outcomes, legacy_outcomes);
            assert_eq!(
                legacy_work.handler_invocations - internal_tool_dispatch_work().handler_invocations,
                2
            );
            assert_eq!(
                legacy_work.argument_clones - internal_tool_dispatch_work().argument_clones,
                2
            );
            harness.shutdown().expect("shutdown");
        }
    }
}

/// Unknown starts must visit no handler or payload, while cancellation remains
/// a broadcast correlation event in original registration order.
#[test]
fn unknown_starts_are_ignored_and_non_start_correlation_still_broadcasts() {
    let temp = TempDir::new().expect("tempdir");
    let (mut harness, _conversation_id, agent_id, visits) =
        internal_dispatch_harness(temp.path(), StartReaction::Record);
    let unknown = Event::ToolStarted(tau_proto::ToolStarted {
        call_id: "unknown-large".into(),
        tool_name: ToolName::new("unknown"),
        arguments: CborValue::Bytes(vec![0x7b; 8 * 1024 * 1024]),
        agent_id,
        originator: tau_proto::PromptOriginator::User,
        invocation_policy: tau_proto::ToolInvocationPolicy::default(),
    });
    reset_internal_tool_dispatch_work();
    harness
        .dispatch_internal_tool_event(&unknown)
        .expect("unknown start ignored");
    assert_eq!(
        internal_tool_dispatch_work(),
        InternalToolDispatchWork::default()
    );
    assert!(visits.lock().expect("visit log").is_empty());

    let unowned_call_id = ToolCallId::from("known-but-external");
    harness.tool_routing.tool_runtime.pending_tools.insert(
        unowned_call_id.clone(),
        PendingTool {
            name: ToolName::new("external"),
            internal_name: ToolName::new("external"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    let unowned = Event::ToolStarted(tau_proto::ToolStarted {
        call_id: unowned_call_id,
        tool_name: ToolName::new("external"),
        arguments: CborValue::Bytes(vec![0x6c; 8 * 1024 * 1024]),
        agent_id: tau_proto::AgentId::parse("external-agent").expect("test agent id"),
        originator: tau_proto::PromptOriginator::User,
        invocation_policy: tau_proto::ToolInvocationPolicy::default(),
    });
    harness
        .dispatch_internal_tool_event(&unowned)
        .expect("unowned start ignored");
    assert_eq!(
        internal_tool_dispatch_work(),
        InternalToolDispatchWork {
            ownership_predicate_visits: 4,
            handler_invocations: 0,
            argument_clones: 0,
        }
    );
    assert!(visits.lock().expect("visit log").is_empty());

    let late_call_id = ToolCallId::from("late-target");
    harness.tool_routing.tool_runtime.pending_tools.insert(
        late_call_id.clone(),
        PendingTool {
            name: ToolName::new("visible_target"),
            internal_name: ToolName::new("target"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    let late = Event::ToolStarted(tau_proto::ToolStarted {
        call_id: late_call_id,
        tool_name: ToolName::new("visible_target"),
        arguments: CborValue::Bytes(vec![0x4d; 8 * 1024 * 1024]),
        agent_id: tau_proto::AgentId::parse("late-agent").expect("test agent id"),
        originator: tau_proto::PromptOriginator::User,
        invocation_policy: tau_proto::ToolInvocationPolicy::default(),
    });
    reset_internal_tool_dispatch_work();
    harness
        .dispatch_internal_tool_event(&late)
        .expect("late start remains harmless");
    assert_eq!(
        internal_tool_dispatch_work(),
        InternalToolDispatchWork {
            ownership_predicate_visits: 4,
            handler_invocations: 2,
            argument_clones: 0,
        }
    );
    assert_eq!(
        *visits.lock().expect("visit log"),
        ["owner-a:start", "owner-b:start"]
    );
    let Event::ToolStarted(late_started) = &late else {
        unreachable!("fixture always builds a start");
    };
    let (legacy_outcomes, legacy_work) = legacy_started_dispatch(late_started, false);
    assert!(legacy_outcomes.is_empty());
    assert_eq!(
        legacy_work,
        LegacyStartedDispatchWork {
            handler_invocations: 4,
            argument_clones: 0,
        }
    );

    visits.lock().expect("visit log").clear();
    reset_internal_tool_dispatch_work();
    let cancel = Event::ToolCancelRequest(tau_proto::ToolCancelRequest {
        target_call_id: "unknown-large".into(),
    });
    harness
        .dispatch_internal_tool_event(&cancel)
        .expect("cancel broadcast");
    assert_eq!(
        internal_tool_dispatch_work(),
        InternalToolDispatchWork {
            ownership_predicate_visits: 0,
            handler_invocations: 4,
            argument_clones: 0,
        }
    );
    assert_eq!(
        *visits.lock().expect("visit log"),
        [
            "non-owner-a:cancel",
            "owner-a:cancel",
            "owner-b:cancel",
            "non-owner-b:cancel",
        ]
    );
    harness.shutdown().expect("shutdown");
}

/// Agent-only materialization must clone one selected payload, while the same
/// peer correlation cannot acquire model-agent ownership or clone arguments.
#[test]
fn agent_owned_materializer_clones_only_model_owned_large_payloads() {
    for peer_owned in [false, true] {
        let temp = TempDir::new().expect("tempdir");
        let (mut harness, conversation_id, agent_id, _visits) =
            internal_dispatch_harness(temp.path(), StartReaction::Record);
        let event = correlated_start(
            &mut harness,
            &conversation_id,
            &agent_id,
            &format!("agent-owned-{peer_owned}"),
            8 * 1024 * 1024,
            peer_owned,
        );
        let Event::ToolStarted(started) = &event else {
            unreachable!("fixture always builds a start");
        };
        reset_internal_tool_dispatch_work();
        let owner =
            crate::InternalToolHost::new(&mut harness).agent_owned_internal_started_call(started);
        if peer_owned {
            assert!(owner.is_none());
            assert_eq!(internal_tool_dispatch_work().argument_clones, 0);
        } else {
            let owner = owner.expect("model-agent ownership");
            assert_eq!(owner.call().name.as_str(), "target");
            assert_eq!(owner.visible_tool_name().as_str(), "visible_target");
            assert!(
                matches!(&owner.call().arguments, CborValue::Bytes(bytes) if bytes.len() == 8 * 1024 * 1024)
            );
            assert_eq!(internal_tool_dispatch_work().argument_clones, 1);
        }
        harness.shutdown().expect("shutdown");
    }
}

/// Selection must finish before callbacks so nested publication cannot borrow
/// invalid handler state, and handler errors must retain ordered short-circuit.
#[test]
fn selected_dispatch_preserves_reentrancy_and_error_short_circuit() {
    for (reaction, expected_visits, expected_work, expects_error) in [
        (
            StartReaction::PublishNotice,
            vec![
                "owner-a:start",
                "non-owner-a:notice",
                "owner-a:notice",
                "owner-b:notice",
                "non-owner-b:notice",
                "owner-b:start",
            ],
            InternalToolDispatchWork {
                ownership_predicate_visits: 4,
                handler_invocations: 6,
                argument_clones: 2,
            },
            false,
        ),
        (
            StartReaction::Error,
            vec!["owner-a:start"],
            InternalToolDispatchWork {
                ownership_predicate_visits: 4,
                handler_invocations: 1,
                argument_clones: 1,
            },
            true,
        ),
    ] {
        let temp = TempDir::new().expect("tempdir");
        let (mut harness, conversation_id, agent_id, visits) =
            internal_dispatch_harness(temp.path(), reaction);
        let event = correlated_start(
            &mut harness,
            &conversation_id,
            &agent_id,
            "reentrant-or-error",
            1024 * 1024,
            false,
        );
        reset_internal_tool_dispatch_work();
        let result = harness.dispatch_internal_tool_event(&event);
        assert_eq!(result.is_err(), expects_error);
        assert_eq!(internal_tool_dispatch_work(), expected_work);
        assert_eq!(*visits.lock().expect("visit log"), expected_visits);
        harness.shutdown().expect("shutdown");
    }
}
