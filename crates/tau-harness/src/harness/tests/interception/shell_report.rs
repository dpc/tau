use std::num as path_std_num;

use super::*;
use crate::harness::{PendingUiShellCommand, UiShellRouteId};
use crate::{
    debug_log as path_crate_debug_log, event as path_crate_event,
    event_log as path_crate_event_log, extension as path_crate_extension,
};

/// Register one configured Tool/Core peer as the only generic shell provider.
fn register_shell_provider(harness: &mut Harness, source: &str, kind: tau_proto::ClientKind) {
    for provider in crate::harness::ui_shell_provider_ids(&harness.registry) {
        harness.registry.unregister_connection(&provider);
    }
    connect_ready_configured_extension(harness, source, source, kind);
    harness
        .bus
        .set_subscriptions(
            &crate::test_connection_id(source),
            Vec::new(),
            vec![EventSelector::Exact(tau_proto::EventName::UI_SHELL_COMMAND)],
        )
        .expect("subscribe shell provider");
    harness.registry.register(
        &crate::test_connection_id(source),
        tau_proto::ToolSpec {
            name: tau_proto::ToolName::new("shell"),
            model_visible_name: None,
            description: None,
            tool_type: tau_proto::ToolType::Function,
            parameters: None,
            format: None,
            tags: vec![tau_proto::ToolTag::new("shell:exec:generic")],
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
}

/// Register a shell provider and create the default loaded user agent.
fn seed_shell_provider_and_default_agent(
    harness: &mut Harness,
    source: &str,
    kind: tau_proto::ClientKind,
) -> tau_proto::AgentId {
    register_shell_provider(harness, source, kind);
    let cid = ensure_test_user_agent(harness);
    crate::parse_agent_id(
        harness.agents[&cid]
            .agent_id
            .as_deref()
            .expect("durable agent id"),
    )
}

/// Register one configured Tool/Core peer as the only generic shell provider
/// and route a user-shell command to it.
fn seed_routed_shell_command(
    harness: &mut Harness,
    source: &str,
    kind: tau_proto::ClientKind,
    ui_command_id: &str,
    include_in_context: bool,
) -> (tau_proto::UiShellCommand, UiShellRouteId) {
    let agent_id = seed_shell_provider_and_default_agent(harness, source, kind);
    let command = tau_proto::UiShellCommand {
        session_id: harness.current_session_id.clone(),
        command_id: test_shell_command_id(ui_command_id),
        command: "pwd".to_owned(),
        include_in_context,
        target_agent_id: Some(agent_id),
    };
    harness.handle_ui_shell_command(&crate::test_connection_id("ui"), command.clone());
    let route_id = harness
        .ui_runtime
        .pending_ui_shell_commands
        .keys()
        .next()
        .expect("routed shell command")
        .clone();
    (command, route_id)
}

/// A targetless command without a loaded agent must retain its explicit
/// no-agent identity from the start through its unroutable terminal.
#[test]
fn targetless_unroutable_shell_start_and_terminal_keep_none_target() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let ui_sink = connect_test_client(&mut harness, "shell-ui", tau_proto::ClientKind::Ui);
    harness
        .bus
        .set_subscriptions(
            &crate::test_connection_id("shell-ui"),
            Vec::new(),
            vec![EventSelector::Prefix("shell.".to_owned())],
        )
        .expect("subscribe shell UI");
    register_shell_provider(&mut harness, "shell-owner", tau_proto::ClientKind::Tool);
    let _ = ensure_test_user_agent(&mut harness);
    for conversation in harness.agents.values_mut() {
        if conversation.originator.is_user() {
            conversation.terminating = true;
        }
    }
    let command = tau_proto::UiShellCommand {
        session_id: harness.current_session_id.clone(),
        command_id: test_shell_command_id("targetless-unroutable-shell"),
        command: "pwd".to_owned(),
        include_in_context: false,
        target_agent_id: None,
    };

    harness.handle_ui_shell_command(&crate::test_connection_id("ui"), command.clone());

    let lifecycle = ui_sink
        .lock()
        .expect("UI sink")
        .iter()
        .filter_map(|routed| peel_inner_event(&routed.frame))
        .filter_map(|event| match event {
            Event::UiShellCommand(start) if start.command_id == command.command_id => {
                Some((start.command_id.clone(), start.target_agent_id.clone()))
            }
            Event::ShellCommandFinished(finished) if finished.command_id == command.command_id => {
                Some((
                    finished.command_id.clone(),
                    finished.target_agent_id.clone(),
                ))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle,
        [
            (command.command_id.clone(), None),
            (command.command_id, None),
        ],
        "the UI must receive one internally consistent unroutable lifecycle"
    );
}

/// A targetless start must use the harness-resolved default-agent identity
/// through its terminal, so one UI renderer can retire its running block.
#[test]
fn targetless_shell_start_and_terminal_share_resolved_default_agent() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let ui_sink = connect_test_client(&mut harness, "shell-ui", tau_proto::ClientKind::Ui);
    harness
        .bus
        .set_subscriptions(
            &crate::test_connection_id("shell-ui"),
            Vec::new(),
            vec![EventSelector::Prefix("shell.".to_owned())],
        )
        .expect("subscribe shell UI");
    let default_agent_id = seed_shell_provider_and_default_agent(
        &mut harness,
        "shell-owner",
        tau_proto::ClientKind::Tool,
    );
    let command = tau_proto::UiShellCommand {
        session_id: harness.current_session_id.clone(),
        command_id: test_shell_command_id("targetless-shell"),
        command: "pwd".to_owned(),
        include_in_context: false,
        target_agent_id: None,
    };

    harness.handle_ui_shell_command(&crate::test_connection_id("ui"), command.clone());

    let route_id = harness
        .ui_runtime
        .pending_ui_shell_commands
        .keys()
        .next()
        .expect("routed shell command")
        .clone();
    let routed = harness
        .ui_runtime
        .pending_ui_shell_commands
        .get(&route_id)
        .expect("pending route")
        .command
        .clone();
    assert_eq!(routed.command_id, command.command_id);
    assert_eq!(routed.target_agent_id, Some(default_agent_id.clone()));

    let starts = ui_sink
        .lock()
        .expect("UI sink")
        .iter()
        .filter_map(|routed| peel_inner_event(&routed.frame))
        .filter_map(|event| match event {
            Event::UiShellCommand(start) if start.command_id == command.command_id => {
                Some(start.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        starts.as_slice(),
        std::slice::from_ref(&routed),
        "the UI start must carry the resolved owner"
    );

    harness
        .handle_extension_event(
            "shell-owner",
            TestProtocolItem::Event(finished_report(&route_id, &routed, "done")),
        )
        .expect("commit terminal shell report");

    let terminals = ui_sink
        .lock()
        .expect("UI sink")
        .iter()
        .filter_map(|routed| peel_inner_event(&routed.frame))
        .filter_map(|event| match event {
            Event::ShellCommandFinished(finished) if finished.command_id == command.command_id => {
                Some(finished.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        terminals.as_slice(),
        [finished]
            if finished.command_id == command.command_id
                && finished.target_agent_id == Some(default_agent_id)
    ));
}

/// Build one progress report using the private provider route id.
fn progress_report(
    route_id: &UiShellRouteId,
    target_agent_id: Option<tau_proto::AgentId>,
    chunk: &str,
) -> Event {
    Event::ShellCommandProgressReported(tau_proto::ShellCommandProgress {
        command_id: route_id.as_protocol_id().clone(),
        stream: tau_proto::ShellStream::Stdout,
        chunk: chunk.to_owned(),
        target_agent_id,
    })
}

/// Build one terminal report by echoing the immutable routed request identity.
fn finished_report(
    route_id: &UiShellRouteId,
    command: &tau_proto::UiShellCommand,
    output: &str,
) -> Event {
    Event::ShellCommandFinishedReported(tau_proto::ShellCommandFinished {
        command_id: route_id.as_protocol_id().clone(),
        session_id: command.session_id.clone(),
        command: command.command.clone(),
        include_in_context: command.include_in_context,
        target_agent_id: command.target_agent_id.clone(),
        output: output.to_owned(),
        exit_code: Some(0),
        cancelled: false,
    })
}

/// Collect shell reports, canonical facts, and user-shell transcript injection
/// in their runtime commit order.
fn committed_shell_events(harness: &Harness) -> Vec<(Option<tau_proto::ConnectionId>, Event)> {
    let mut events = Vec::new();
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = harness.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches!(
            entry.event,
            Event::ShellCommandProgressReported(_)
                | Event::ShellCommandProgress(_)
                | Event::ShellCommandFinishedReported(_)
                | Event::ShellCommandFinished(_)
                | Event::AgentUserMessageInjected(_)
        ) {
            events.push((entry.source, entry.event));
        }
    }
    events
}

/// A Tool-authored terminal report commits before its protected canonical fact,
/// and transcript injection waits for that canonical commit.
#[test]
fn terminal_report_commits_before_canonical_completion_and_injection() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let (command, route_id) = seed_routed_shell_command(
        &mut harness,
        "shell-owner",
        tau_proto::ClientKind::Tool,
        "shell-terminal",
        true,
    );
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![
                    EventSelector::Exact(tau_proto::EventName::SHELL_COMMAND_FINISHED_REPORTED),
                    EventSelector::Exact(tau_proto::EventName::SHELL_COMMAND_FINISHED),
                ],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register shell interceptor");

    harness
        .handle_extension_event(
            "shell-owner",
            TestProtocolItem::Event(finished_report(&route_id, &command, "original")),
        )
        .expect("park report");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(finished_report(
                    &route_id,
                    &command,
                    "replacement",
                )))),
            })),
        )
        .expect("commit replacement report");

    let committed = committed_shell_events(&harness);
    assert!(
        matches!(
            committed.as_slice(),
            [(Some(source), Event::ShellCommandFinishedReported(finished))]
                if source == "shell-owner" && finished.output == "replacement"
        ),
        "{committed:#?}"
    );
    assert!(matches!(
        harness
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::ShellCommandFinished(_))
    ));

    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("canonical completion must pass");

    assert!(matches!(
        committed_shell_events(&harness).as_slice(),
        [
            (Some(report_source), Event::ShellCommandFinishedReported(report)),
            (Some(canonical_source), Event::ShellCommandFinished(canonical)),
            (_, Event::AgentUserMessageInjected(injected)),
        ] if report_source == "shell-owner"
            && canonical_source == HARNESS_CONNECTION_ID
            && report.output == "replacement"
            && canonical.command_id == command.command_id
            && canonical.output == "replacement"
             && injected.text.contains("replacement")
    ));
    assert!(
        loaded_agent_events(&harness, command.session_id.as_str())
            .iter()
            .any(|event| matches!(
                event,
                Event::ShellCommandFinished(finished)
                    if finished.command_id == command.command_id
                        && finished.output == "replacement"
            ))
    );
}

/// A UI-only `!!` completion remains live even when its provider report is
/// accepted, so attach replay cannot expose commands excluded from context.
#[test]
fn ui_only_terminal_completion_does_not_enter_target_journal() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let (command, route_id) = seed_routed_shell_command(
        &mut harness,
        "shell-owner",
        tau_proto::ClientKind::Tool,
        "shell-ui-only",
        false,
    );

    harness
        .handle_extension_event(
            "shell-owner",
            TestProtocolItem::Event(finished_report(&route_id, &command, "ui-only")),
        )
        .expect("commit UI-only shell completion");

    assert!(committed_shell_events(&harness).iter().any(|(_, event)| {
        matches!(
            event,
            Event::ShellCommandFinished(finished)
                if finished.command_id == command.command_id
        )
    }));
    assert!(
        !loaded_agent_events(&harness, command.session_id.as_str())
            .iter()
            .any(|event| matches!(event, Event::ShellCommandFinished(_)))
    );
}

/// Attach catch-up snapshots one empty running lifecycle and the live terminal
/// settles that same public command id once without replaying progress history.
#[test]
fn late_ui_snapshots_running_shell_then_receives_one_live_completion() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let (command, route_id) = seed_routed_shell_command(
        &mut harness,
        "shell-owner",
        tau_proto::ClientKind::Tool,
        "shell-late-ui",
        true,
    );
    harness
        .handle_extension_event(
            "shell-owner",
            TestProtocolItem::Event(progress_report(
                &route_id,
                command.target_agent_id.clone(),
                "transient chunk",
            )),
        )
        .expect("commit transient progress");

    let sink = connect_test_client(&mut harness, "late-shell-ui", tau_proto::ClientKind::Ui);
    harness
        .handle_client_event(
            "late-shell-ui",
            TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
                historical_selectors: vec![
                    EventSelector::Exact(tau_proto::EventName::UI_SHELL_COMMAND),
                    EventSelector::Prefix("shell.".to_owned()),
                ],
                live_selectors: vec![EventSelector::Prefix("shell.".to_owned())],
            })),
        )
        .expect("subscribe late UI");

    let replayed = sink
        .lock()
        .expect("sink")
        .iter()
        .filter_map(|routed| match &routed.frame {
            HarnessOutputMessage::Deliver(delivery) if delivery.replay => {
                Some((*delivery.event).clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        replayed.as_slice(),
        [Event::UiShellCommand(snapshot)]
            if snapshot == &command
    ));

    harness
        .handle_extension_event(
            "shell-owner",
            TestProtocolItem::Event(finished_report(&route_id, &command, "completed")),
        )
        .expect("complete snapshotted shell");

    let live_terminals = sink
        .lock()
        .expect("sink")
        .iter()
        .filter(|routed| {
            matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if !delivery.replay
                        && matches!(&*delivery.event, Event::ShellCommandFinished(finished)
                            if finished.command_id == command.command_id
                                && finished.output == "completed")
            )
        })
        .count();
    assert_eq!(live_terminals, 1);
}

/// Live routing remains uncapped while UI catch-up projects a bounded,
/// selector-sensitive current-state snapshot.
#[test]
fn running_shell_snapshot_bounds_only_attach_projection() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let (first, _) = seed_routed_shell_command(
        &mut harness,
        "shell-owner",
        tau_proto::ClientKind::Tool,
        "shell-000",
        false,
    );
    for index in 1..129 {
        let mut command = first.clone();
        command.command_id = test_shell_command_id(format!("shell-{index:03}"));
        harness.handle_ui_shell_command(&crate::test_connection_id("ui"), command);
    }
    assert_eq!(
        harness.ui_runtime.pending_ui_shell_commands.len(),
        129,
        "live route admission must not inherit the replay cap"
    );

    let cases = [
        (
            "snapshot-shell-notice",
            tau_proto::ClientKind::Ui,
            vec![
                EventSelector::Exact(tau_proto::EventName::UI_SHELL_COMMAND),
                EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE),
            ],
            128,
            1,
        ),
        (
            "snapshot-shell-only",
            tau_proto::ClientKind::Ui,
            vec![EventSelector::Exact(tau_proto::EventName::UI_SHELL_COMMAND)],
            128,
            0,
        ),
        (
            "snapshot-notice-only",
            tau_proto::ClientKind::Ui,
            vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            0,
            0,
        ),
        (
            "snapshot-non-ui",
            tau_proto::ClientKind::Tool,
            vec![
                EventSelector::Exact(tau_proto::EventName::UI_SHELL_COMMAND),
                EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE),
            ],
            0,
            0,
        ),
    ];
    for (name, kind, selectors, expected_shells, expected_notices) in cases {
        let sink = connect_test_client(&mut harness, name, kind);
        harness
            .handle_client_event(
                name,
                TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
                    historical_selectors: selectors,
                    live_selectors: Vec::new(),
                })),
            )
            .expect("subscribe snapshot client");
        let sink = sink.lock().expect("sink");
        let replayed = sink.iter().filter_map(|routed| match &routed.frame {
            HarnessOutputMessage::Deliver(delivery) if delivery.replay => {
                Some(delivery.event.as_ref())
            }
            _ => None,
        });
        let shell_ids = replayed
            .clone()
            .filter_map(|event| match event {
                Event::UiShellCommand(command) => Some(command.command_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let notices = replayed
            .filter_map(|event| match event {
                Event::HarnessNotice(notice)
                    if notice.kind == tau_proto::notice_kind::HARNESS_SHELL_SNAPSHOT_OMITTED =>
                {
                    Some(notice)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(shell_ids.len(), expected_shells, "{name}");
        if expected_shells != 0 {
            assert_eq!(
                shell_ids,
                (0..128)
                    .map(|index| format!("shell-{index:03}"))
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(notices.len(), expected_notices, "{name}");
        if let Some(notice) = notices.first() {
            assert!(notice.message.contains("omitted 1 route(s)"));
        }
    }
}

/// A failed target-journal append still settles one live terminal while
/// suppressing context injection, replay, leaked lifecycle state, and
/// duplicates.
#[test]
fn terminal_append_failure_settles_live_without_durable_side_effects() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let sink = connect_test_client(&mut harness, "shell-ui", tau_proto::ClientKind::Ui);
    harness
        .handle_client_event(
            "shell-ui",
            TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
                historical_selectors: Vec::new(),
                live_selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::SHELL_COMMAND_FINISHED,
                )],
            })),
        )
        .expect("subscribe to shell completions");
    let (command, route_id) = seed_routed_shell_command(
        &mut harness,
        "shell-owner",
        tau_proto::ClientKind::Tool,
        "shell-store-fault",
        true,
    );
    let agent_id = command.target_agent_id.as_ref().expect("resolved target");
    let journal = harness
        .state_dir
        .join("agents")
        .join(agent_id.as_str())
        .join("events.cbor");
    let backup = journal.with_extension("cbor.test-backup");
    std::fs::rename(&journal, &backup).expect("park target journal");
    std::fs::create_dir(&journal).expect("block target journal path");
    let report = finished_report(&route_id, &command, "unpersisted");

    harness
        .handle_extension_event("shell-owner", TestProtocolItem::Event(report.clone()))
        .expect("report remains an observation despite append failure");
    harness
        .handle_extension_event("shell-owner", TestProtocolItem::Event(report))
        .expect("late duplicate remains an observation");

    let live = sink
        .lock()
        .expect("sink")
        .iter()
        .filter(|routed| {
            matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if !delivery.replay
                        && matches!(&*delivery.event, Event::ShellCommandFinished(finished)
                            if finished.command_id == command.command_id
                                && finished.output == "unpersisted")
            )
        })
        .count();
    assert_eq!(live, 1);
    assert!(
        !harness
            .ui_runtime
            .active_ui_shell_command_ids
            .contains(&command.command_id)
    );
    assert!(
        !harness
            .ui_runtime
            .pending_ui_shell_output_injections
            .contains(&command.command_id)
    );
    assert!(
        !harness
            .ui_runtime
            .pending_ui_shell_commands
            .contains_key(&route_id)
    );
    assert!(
        !loaded_agent_events(&harness, command.session_id.as_str())
            .iter()
            .any(|event| matches!(
                event,
                Event::ShellCommandFinished(_) | Event::AgentUserMessageInjected(_)
            ))
    );

    std::fs::remove_dir(&journal).expect("remove journal blocker");
    std::fs::rename(&backup, &journal).expect("restore target journal");
}

/// A Core shell provider uses the same report boundary, with the harness
/// mapping the private route id back to the public UI lifecycle id.
#[test]
fn core_progress_report_derives_harness_sourced_canonical_progress() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let (command, route_id) = seed_routed_shell_command(
        &mut harness,
        "core-shell",
        tau_proto::ClientKind::Core,
        "shell-progress",
        false,
    );

    harness
        .handle_extension_event(
            "core-shell",
            TestProtocolItem::Event(progress_report(
                &route_id,
                command.target_agent_id.clone(),
                "chunk",
            )),
        )
        .expect("commit progress report");

    assert!(matches!(
        committed_shell_events(&harness).as_slice(),
        [
            (Some(report_source), Event::ShellCommandProgressReported(report)),
            (Some(canonical_source), Event::ShellCommandProgress(canonical)),
        ] if report_source == "core-shell"
            && canonical_source == HARNESS_CONNECTION_ID
            && report.command_id == *route_id.as_protocol_id()
            && canonical.command_id == command.command_id
            && canonical.chunk == "chunk"
    ));
}

/// Configured non-Tool/Core peers cannot publish reports, and Tool/Core peers
/// cannot directly author either canonical shell fact.
#[test]
fn shell_report_authority_rejects_wrong_kind_and_canonical_spoofs() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let (command, route_id) = seed_routed_shell_command(
        &mut harness,
        "shell-owner",
        tau_proto::ClientKind::Tool,
        "shell-authority",
        false,
    );
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    let report = progress_report(&route_id, command.target_agent_id.clone(), "forged");

    harness
        .handle_extension_event("provider", TestProtocolItem::Event(report))
        .expect("reject wrong-kind report");
    harness
        .handle_extension_event(
            "shell-owner",
            TestProtocolItem::Event(Event::ShellCommandProgress(
                tau_proto::ShellCommandProgress {
                    command_id: command.command_id.clone(),
                    stream: tau_proto::ShellStream::Stdout,
                    chunk: "forged canonical".to_owned(),
                    target_agent_id: command.target_agent_id,
                },
            )),
        )
        .expect("reject canonical spoof");

    assert!(committed_shell_events(&harness).is_empty());
    assert_eq!(harness.ui_runtime.pending_ui_shell_commands.len(), 1);
}

/// A report parked across an extension generation replacement remains an
/// observable report but cannot consume the successor's route or derive a fact.
#[test]
fn parked_stale_shell_report_cannot_publish_canonical_fact() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let (command, route_id) = seed_routed_shell_command(
        &mut harness,
        "shell-owner",
        tau_proto::ClientKind::Tool,
        "shell-stale",
        false,
    );
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::SHELL_COMMAND_PROGRESS_REPORTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register report interceptor");

    harness
        .handle_extension_event(
            "shell-owner",
            TestProtocolItem::Event(progress_report(&route_id, command.target_agent_id, "stale")),
        )
        .expect("park report");
    harness
        .extensions
        .entries
        .get_mut("shell-owner")
        .expect("replacement generation")
        .instance_id = tau_proto::ExtensionInstanceId::new(43);
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("commit stale report");

    assert!(matches!(
        committed_shell_events(&harness).as_slice(),
        [(Some(source), Event::ShellCommandProgressReported(_))]
            if source == "shell-owner"
    ));
    assert_eq!(harness.ui_runtime.pending_ui_shell_commands.len(), 1);
}

/// Immutable publication context preserves an ephemeral original route even
/// when interception rewrites the committed report to an unknown route id.
#[test]
fn intercepted_route_replacement_cannot_leak_ephemeral_report_to_debug_jsonl() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let (command, route_id) = seed_routed_shell_command(
        &mut harness,
        "shell-owner",
        tau_proto::ClientKind::Tool,
        "shell-replaced-route",
        false,
    );
    harness
        .ui_runtime
        .pending_ui_shell_commands
        .get_mut(&route_id)
        .expect("pending route")
        .targets_ephemeral = true;
    harness
        .ui_runtime
        .ephemeral_ui_shell_route_ids
        .insert(route_id.clone());
    let debug_dir = tmp.path().join("debug");
    harness.debug_log =
        Some(path_crate_debug_log::DebugEventLog::open(&debug_dir).expect("open debug log"));
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::SHELL_COMMAND_PROGRESS_REPORTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register report interceptor");

    harness
        .handle_extension_event(
            "shell-owner",
            TestProtocolItem::Event(progress_report(
                &route_id,
                command.target_agent_id.clone(),
                "private output",
            )),
        )
        .expect("park report");
    let replacement_secret = "private replacement output";
    let reply = InterceptReply {
        action: InterceptAction::Pass(Some(Box::new(progress_report(
            &UiShellRouteId::new(test_shell_command_id("unknown-route")),
            command.target_agent_id,
            replacement_secret,
        )))),
    };
    harness.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id("interceptor"),
        tau_proto::HarnessInputMessage::InterceptReply(reply.clone()),
    ));
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(reply)),
        )
        .expect("commit replaced report");

    assert!(matches!(
        committed_shell_events(&harness).as_slice(),
        [(Some(source), Event::ShellCommandProgressReported(progress))]
            if source == "shell-owner" && progress.command_id.as_str() == "unknown-route"
    ));
    let jsonl = std::fs::read_to_string(debug_dir.join("events.jsonl")).expect("read debug log");
    assert!(
        !jsonl.contains("\"event_name\":\"shell.command_progress_reported\""),
        "immutable original-route classification must suppress the replaced report"
    );
    assert!(
        !jsonl.contains(replacement_secret),
        "raw intercept reply must use the pending publication's immutable classification"
    );
}

/// Immutable original-route privacy survives multiple interceptor replacements,
/// so a later reply cannot log an earlier ephemeral report's replacement
/// output.
#[test]
fn multi_interceptor_replacements_keep_raw_reply_ephemeral_suppression() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let (command, route_id) = seed_routed_shell_command(
        &mut harness,
        "shell-owner",
        tau_proto::ClientKind::Tool,
        "shell-multi-replacement",
        false,
    );
    harness
        .ui_runtime
        .pending_ui_shell_commands
        .get_mut(&route_id)
        .expect("pending route")
        .targets_ephemeral = true;
    harness
        .ui_runtime
        .ephemeral_ui_shell_route_ids
        .insert(route_id.clone());
    let debug_dir = tmp.path().join("debug");
    harness.debug_log =
        Some(path_crate_debug_log::DebugEventLog::open(&debug_dir).expect("open debug log"));
    for (interceptor, priority) in [("interceptor-a", 0), ("interceptor-b", 1)] {
        connect_test_tool(&mut harness, interceptor);
        harness
            .handle_extension_event(
                interceptor,
                TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                    selectors: vec![EventSelector::Exact(
                        tau_proto::EventName::SHELL_COMMAND_PROGRESS_REPORTED,
                    )],
                    priority: InterceptionPriority::new(priority),
                })),
            )
            .expect("register report interceptor");
    }

    harness
        .handle_extension_event(
            "shell-owner",
            TestProtocolItem::Event(progress_report(
                &route_id,
                command.target_agent_id.clone(),
                "original private output",
            )),
        )
        .expect("park original report");
    let unknown_route = UiShellRouteId::new(test_shell_command_id("unknown-route"));
    let first_secret = "first private replacement";
    let first_reply = InterceptReply {
        action: InterceptAction::Pass(Some(Box::new(progress_report(
            &unknown_route,
            command.target_agent_id.clone(),
            first_secret,
        )))),
    };
    harness.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id("interceptor-a"),
        tau_proto::HarnessInputMessage::InterceptReply(first_reply.clone()),
    ));
    harness
        .handle_extension_event(
            "interceptor-a",
            TestProtocolItem::Message(TestMessage::InterceptReply(first_reply)),
        )
        .expect("advance to second interceptor");

    let second_secret = "second private replacement";
    let second_reply = InterceptReply {
        action: InterceptAction::Pass(Some(Box::new(progress_report(
            &unknown_route,
            command.target_agent_id,
            second_secret,
        )))),
    };
    harness.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id("interceptor-b"),
        tau_proto::HarnessInputMessage::InterceptReply(second_reply.clone()),
    ));
    harness
        .handle_extension_event(
            "interceptor-b",
            TestProtocolItem::Message(TestMessage::InterceptReply(second_reply)),
        )
        .expect("commit second replacement");

    let jsonl = std::fs::read_to_string(debug_dir.join("events.jsonl")).expect("read debug log");
    assert!(!jsonl.contains(first_secret));
    assert!(!jsonl.contains(second_secret));
}

/// A canonical shell replacement cannot forge an ephemeral target to suppress
/// the raw reply audit when its harness-owned UI id is durable.
#[test]
fn canonical_shell_replacement_target_cannot_suppress_raw_reply_audit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let forged_ephemeral_target =
        crate::parse_agent_id(harness.agents[&cid].agent_id.as_deref().expect("agent id"));
    harness.agents.get_mut(&cid).expect("agent").persistence =
        tau_core::AgentPersistenceMode::Ephemeral;
    let debug_dir = tmp.path().join("debug");
    harness.debug_log =
        Some(path_crate_debug_log::DebugEventLog::open(&debug_dir).expect("open debug log"));
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::SHELL_COMMAND_PROGRESS,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register canonical interceptor");
    let command_id: tau_proto::ShellCommandId = test_shell_command_id("durable-canonical-ui-id");
    harness.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::ShellCommandProgress(tau_proto::ShellCommandProgress {
            command_id: command_id.clone(),
            stream: tau_proto::ShellStream::Stdout,
            chunk: "original".to_owned(),
            target_agent_id: None,
        }),
    );
    let replacement_secret = "auditable durable replacement";
    let reply = InterceptReply {
        action: InterceptAction::Pass(Some(Box::new(Event::ShellCommandProgress(
            tau_proto::ShellCommandProgress {
                command_id,
                stream: tau_proto::ShellStream::Stderr,
                chunk: replacement_secret.to_owned(),
                target_agent_id: Some(forged_ephemeral_target),
            },
        )))),
    };
    harness.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id("interceptor"),
        tau_proto::HarnessInputMessage::InterceptReply(reply.clone()),
    ));
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(reply)),
        )
        .expect("commit durable replacement");

    let jsonl = std::fs::read_to_string(debug_dir.join("events.jsonl")).expect("read debug log");
    assert!(jsonl.contains(replacement_secret));
}

/// Pre-Ready shell reports remain bounded operational traffic and preserve
/// activation ordering before their report/canonical pair is released.
#[test]
fn pre_ready_shell_report_is_deferred_until_activation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let (command, route_id) = seed_routed_shell_command(
        &mut harness,
        "shell-owner",
        tau_proto::ClientKind::Tool,
        "shell-deferred",
        false,
    );
    harness
        .extensions
        .entries
        .get_mut("shell-owner")
        .expect("shell owner")
        .state = path_crate_extension::ExtensionState::Handshaking;
    let report = progress_report(&route_id, command.target_agent_id, "deferred");
    let expected_bytes = Harness::encoded_emit_size(&report, false);

    harness
        .handle_extension_event(
            "shell-owner",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(report),
                persist: false,
            })),
        )
        .expect("defer pre-Ready report");
    assert!(committed_shell_events(&harness).is_empty());
    assert_eq!(
        harness.extensions.activation_staging["shell-owner"].retained_message_bytes,
        expected_bytes
    );

    harness
        .handle_extension_message(
            &crate::test_connection_id("shell-owner"),
            TestMessage::Ready(Default::default()),
        )
        .expect("activate shell owner");
    assert!(matches!(
        committed_shell_events(&harness).as_slice(),
        [
            (_, Event::ShellCommandProgressReported(_)),
            (Some(source), Event::ShellCommandProgress(_)),
        ] if source == HARNESS_CONNECTION_ID
    ));
}

/// A pre-Ready report retains its frame-admission session across activation and
/// cannot bind to a replacement-session route that reuses its private id.
#[test]
fn pre_ready_shell_report_cannot_bind_after_session_rollover() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let (mut command, route_id) = seed_routed_shell_command(
        &mut harness,
        "shell-owner",
        tau_proto::ClientKind::Tool,
        "shell-old-session",
        false,
    );
    harness
        .extensions
        .entries
        .get_mut("shell-owner")
        .expect("shell owner")
        .state = path_crate_extension::ExtensionState::Handshaking;
    harness
        .handle_extension_event(
            "shell-owner",
            TestProtocolItem::Event(progress_report(
                &route_id,
                command.target_agent_id.clone(),
                "old session",
            )),
        )
        .expect("defer old-session report");
    harness
        .switch_session(
            "replacement-session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            tau_proto::SessionStartReason::New,
        )
        .expect("switch session");
    command.session_id = harness.current_session_id.clone();
    harness.ui_runtime.pending_ui_shell_commands.insert(
        route_id,
        PendingUiShellCommand {
            provider_id: crate::test_connection_id("shell-owner"),
            command,
            targets_ephemeral: false,
        },
    );

    harness
        .handle_extension_message(
            &crate::test_connection_id("shell-owner"),
            TestMessage::Ready(Default::default()),
        )
        .expect("activate old report");

    assert!(
        committed_shell_events(&harness)
            .iter()
            .any(|(_, event)| matches!(event, Event::ShellCommandProgressReported(_)))
    );
    assert!(
        !committed_shell_events(&harness)
            .iter()
            .any(|(_, event)| matches!(event, Event::ShellCommandProgress(_)))
    );
    assert_eq!(harness.ui_runtime.pending_ui_shell_commands.len(), 1);
}

/// Ephemeral classification uses harness-owned pending-route state rather than
/// a peer-controlled target field.
#[test]
fn shell_report_ephemeral_classification_uses_private_route_identity() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let route_id = UiShellRouteId::new(test_shell_command_id("ephemeral-shell-route"));
    harness.ui_runtime.pending_ui_shell_commands.insert(
        route_id.clone(),
        PendingUiShellCommand {
            provider_id: crate::test_connection_id("shell-owner"),
            command: tau_proto::UiShellCommand {
                session_id: harness.current_session_id.clone(),
                command_id: tau_proto::ShellCommandId::parse("ephemeral-shell-ui")
                    .expect("test identifier must satisfy its grammar"),
                command: "pwd".to_owned(),
                include_in_context: false,
                target_agent_id: None,
            },
            targets_ephemeral: true,
        },
    );
    harness
        .ui_runtime
        .ephemeral_ui_shell_route_ids
        .insert(route_id.clone());
    harness
        .ui_runtime
        .pending_ephemeral_ui_shell_canonical_events
        .insert(
            test_shell_command_id("ephemeral-shell-ui"),
            path_std_num::NonZeroUsize::MIN,
        );
    let report = progress_report(&route_id, None, "private output");
    let canonical = Event::ShellCommandProgress(tau_proto::ShellCommandProgress {
        command_id: tau_proto::ShellCommandId::parse("ephemeral-shell-ui")
            .expect("test identifier must satisfy its grammar"),
        stream: tau_proto::ShellStream::Stdout,
        chunk: "private output".to_owned(),
        target_agent_id: None,
    });

    assert!(harness.event_targets_ephemeral_agent(&report, None));
    assert!(harness.event_targets_ephemeral_agent(&canonical, None));
    assert!(
        harness
            .ui_runtime
            .pending_ui_shell_commands
            .contains_key(&route_id)
    );
}

/// Unknown routes are not misclassified as ephemeral before or after rollover;
/// they retain ordinary durable debug audit treatment.
#[test]
fn unknown_shell_report_route_is_not_classified_ephemeral_across_rollover() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let route_id = UiShellRouteId::new(test_shell_command_id("parked-ephemeral-shell-route"));
    let report = progress_report(&route_id, None, "old private output");
    assert!(!harness.event_targets_ephemeral_agent(&report, None));
    harness
        .switch_session(
            "replacement-session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            tau_proto::SessionStartReason::New,
        )
        .expect("switch session");
    assert!(!harness.event_targets_ephemeral_agent(&report, None));
}

/// Process-lifetime ephemeral route tombstones survive session rollover so a
/// previously admitted or late report cannot leak ephemeral output.
#[test]
fn ephemeral_shell_route_tombstone_survives_session_rollover() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let route_id = UiShellRouteId::new(test_shell_command_id("ephemeral-old-route"));
    harness
        .ui_runtime
        .ephemeral_ui_shell_route_ids
        .insert(route_id.clone());
    let report = progress_report(&route_id, None, "old private output");
    harness
        .switch_session(
            "replacement-session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            tau_proto::SessionStartReason::New,
        )
        .expect("switch session");

    assert!(harness.event_targets_ephemeral_agent(&report, None));
}

/// A report cannot suppress debug JSONL by claiming an unrelated ephemeral
/// target; only the harness-private route identity controls report
/// classification.
#[test]
fn shell_report_ephemeral_classification_ignores_peer_target_claim() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let claimed_agent_id =
        crate::parse_agent_id(harness.agents[&cid].agent_id.as_deref().expect("agent id"));
    harness.agents.get_mut(&cid).expect("agent").persistence =
        tau_core::AgentPersistenceMode::Ephemeral;
    let route_id = UiShellRouteId::new(test_shell_command_id("durable-route"));
    harness.ui_runtime.pending_ui_shell_commands.insert(
        route_id.clone(),
        PendingUiShellCommand {
            provider_id: crate::test_connection_id("shell-owner"),
            command: tau_proto::UiShellCommand {
                session_id: harness.current_session_id.clone(),
                command_id: tau_proto::ShellCommandId::parse("durable-ui-id")
                    .expect("test identifier must satisfy its grammar"),
                command: "pwd".to_owned(),
                include_in_context: false,
                target_agent_id: None,
            },
            targets_ephemeral: false,
        },
    );
    let report = progress_report(&route_id, Some(claimed_agent_id), "untrusted");

    assert!(!harness.event_targets_ephemeral_agent(&report, None));
}

/// Unknown reports retain their ordinary auditable committed debug line and are
/// not mislabeled as ephemeral by a peer-chosen route id.
#[test]
fn unknown_shell_report_retains_ordinary_debug_audit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "shell-peer",
        "shell-peer",
        tau_proto::ClientKind::Tool,
    );
    let debug_dir = tmp.path().join("debug");
    harness.debug_log =
        Some(path_crate_debug_log::DebugEventLog::open(&debug_dir).expect("open debug log"));
    let secret = "secret-output-that-must-not-enter-debug";

    harness
        .handle_extension_event(
            "shell-peer",
            TestProtocolItem::Event(progress_report(
                &UiShellRouteId::new(test_shell_command_id("unknown-route")),
                None,
                secret,
            )),
        )
        .expect("commit unknown report");

    let jsonl = std::fs::read_to_string(debug_dir.join("events.jsonl")).expect("read debug log");
    assert!(jsonl.contains("\"event_name\":\"shell.command_progress_reported\""));
    assert!(jsonl.contains(secret));
}

/// Dropping mutable canonical progress releases its short-lived ephemeral
/// marker so later UI-id reuse receives independent debug classification.
#[test]
fn dropping_canonical_progress_releases_ephemeral_marker() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::SHELL_COMMAND_PROGRESS,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register progress interceptor");
    let command_id: tau_proto::ShellCommandId = test_shell_command_id("reusable-ui-id");
    harness
        .ui_runtime
        .pending_ephemeral_ui_shell_canonical_events
        .insert(command_id.clone(), path_std_num::NonZeroUsize::MIN);
    harness.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::ShellCommandProgress(tau_proto::ShellCommandProgress {
            command_id: command_id.clone(),
            stream: tau_proto::ShellStream::Stdout,
            chunk: "ephemeral".to_owned(),
            target_agent_id: None,
        }),
    );
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("drop progress");

    assert!(
        !harness
            .ui_runtime
            .pending_ephemeral_ui_shell_canonical_events
            .contains_key(&command_id)
    );
}

/// A parked ephemeral progress fact and the rollover failure queued behind it
/// retain independent debug-classification markers for the same UI id.
#[test]
fn parked_progress_and_rollover_terminal_keep_ephemeral_debug_suppression() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let (command, route_id) = seed_routed_shell_command(
        &mut harness,
        "shell-owner",
        tau_proto::ClientKind::Tool,
        "ephemeral-rollover",
        false,
    );
    harness
        .ui_runtime
        .pending_ui_shell_commands
        .get_mut(&route_id)
        .expect("pending route")
        .targets_ephemeral = true;
    harness
        .ui_runtime
        .ephemeral_ui_shell_route_ids
        .insert(route_id.clone());
    let debug_dir = tmp.path().join("debug");
    harness.debug_log =
        Some(path_crate_debug_log::DebugEventLog::open(&debug_dir).expect("open debug log"));
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::SHELL_COMMAND_PROGRESS,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register progress interceptor");

    harness
        .handle_extension_event(
            "shell-owner",
            TestProtocolItem::Event(progress_report(
                &route_id,
                command.target_agent_id,
                "ephemeral output",
            )),
        )
        .expect("park canonical progress");
    harness
        .switch_session(
            "replacement-session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            tau_proto::SessionStartReason::New,
        )
        .expect("queue rollover terminal");
    assert_eq!(
        harness
            .ui_runtime
            .pending_ephemeral_ui_shell_canonical_events
            .get(&command.command_id)
            .map(|count| count.get()),
        None,
        "rollover must cancel the parked progress and commit its queued terminal"
    );
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("consume stale pre-rollover reply");

    assert!(
        !harness
            .ui_runtime
            .pending_ephemeral_ui_shell_canonical_events
            .contains_key(&command.command_id)
    );
    let jsonl = std::fs::read_to_string(debug_dir.join("events.jsonl")).expect("read debug log");
    let shell_lines = jsonl
        .lines()
        .filter(|line| line.contains("\"event_name\":\"shell.command_"))
        .collect::<Vec<_>>();
    assert!(
        shell_lines.is_empty(),
        "ephemeral report and both canonical facts must stay out of debug JSONL: {shell_lines:?}"
    );
}

/// Builds a validated shell command id used by this test module.
fn test_shell_command_id(value: impl AsRef<str>) -> tau_proto::ShellCommandId {
    tau_proto::ShellCommandId::parse(value.as_ref())
        .expect("test identifier must satisfy its grammar")
}
