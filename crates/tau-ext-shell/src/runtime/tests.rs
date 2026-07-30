use super::*;

/// Ensures ToolCancelRequest reaches already-running cancellable tool
/// calls, not just queued scheduler work or shell-only registry
/// entries.
#[test]
fn tool_cancel_request_signals_registered_running_call() {
    let (tx, _rx) = mpsc::channel();
    let runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
    let call_id = tau_proto::ToolCallId::new("running-find");
    let (cancel_tx, cancel_rx) = mpsc::channel();
    runtime
        .running_calls
        .lock()
        .expect("running call registry")
        .insert(call_id.clone(), cancel_tx);

    runtime.handle_tool_cancel_request(tau_proto::ToolCancelRequest {
        target_call_id: call_id,
    });

    cancel_rx
        .recv_timeout(std::time::Duration::from_millis(100))
        .expect("running call cancel signal");
}

/// Ensures runtime shutdown signals registered running cancellable tool
/// calls before scheduler drop waits for worker jobs to exit.
#[test]
fn shutdown_signals_registered_running_call() {
    let (tx, _rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
    let call_id = tau_proto::ToolCallId::new("running-grep");
    let (cancel_tx, cancel_rx) = mpsc::channel();
    runtime
        .running_calls
        .lock()
        .expect("running call registry")
        .insert(call_id, cancel_tx);

    runtime.shutdown();

    cancel_rx
        .recv_timeout(std::time::Duration::from_millis(100))
        .expect("shutdown cancel signal");
}

/// Ensures replayed cwd metadata is folded for later boundary-approved
/// context readiness without emitting replay-time side effects.
#[test]
fn replayed_cwd_metadata_folds_without_emitting_until_live_agent_load() {
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
    let agent_id = tau_proto::AgentId::parse("agent-replay-cwd").expect("agent id");
    let cwd = std::env::current_dir().expect("current dir");

    runtime
        .handle_event(
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: agent_id.clone(),
                key: runtime.cwd_state.key(),
                value: CborValue::Text(cwd.display().to_string()),
                mutation_id: None,
                inheritable: true,
            }),
            true,
        )
        .expect("replay metadata");
    assert!(rx.try_recv().is_err(), "replay fold must not emit output");

    runtime
        .handle_event(
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                    .expect("test identifier must be valid"),

                session_id: "session-1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                agent_id: agent_id.clone(),
                ephemeral: false,
            }),
            false,
        )
        .expect("live load");
    assert!(
        rx.try_recv().is_err(),
        "live load waits for replay boundary before emitting"
    );
    runtime
        .handle_event(
            Event::AgentReplayComplete(tau_proto::AgentReplayComplete {
                agent_id: agent_id.clone(),
                session_id: Some(
                    "session-1"
                        .parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                ),
                error: None,
            }),
            false,
        )
        .expect("replay boundary");

    loop {
        let message = rx.recv().expect("discovery snapshot");
        if matches!(
            message,
            HarnessInputMessage::Emit(ref emit)
                if matches!(emit.event.as_ref(),
                    Event::ExtensionAgentDiscoverySnapshotDeclared(declared)
                        if declared.agent_id == agent_id)
        ) {
            break;
        }
    }
    let HarnessInputMessage::Emit(context) = rx.recv().expect("context publish") else {
        panic!("expected context publish");
    };
    assert!(!context.persist);
    assert!(matches!(
        context.event.as_ref(),
        Event::ExtAgentContextPublish(publish)
            if publish.agent_id == agent_id && publish.key.as_ref() == "workdir"
    ));
    let HarnessInputMessage::Emit(ready) = rx.recv().expect("context ready") else {
        panic!("expected context ready");
    };
    assert!(!ready.persist);
    assert!(matches!(
        ready.event.as_ref(),
        Event::ExtensionContextReady(ready)
            if ready.agent_id == agent_id && ready.session_id == "session-1"
    ));
}

/// Malformed restored workdir metadata is present state, not an absent key
/// that may be overwritten by the process-startup fallback.
#[test]
fn malformed_replayed_workdir_is_retained_without_default_seeding() {
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
    let agent_id = tau_proto::AgentId::parse("agent-invalid-workdir").expect("agent id");
    runtime
        .handle_event(
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                    .expect("test identifier must be valid"),

                session_id: "session-1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                agent_id: agent_id.clone(),
                ephemeral: false,
            }),
            false,
        )
        .expect("load");
    runtime
        .handle_event(
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: agent_id.clone(),
                key: runtime.cwd_state.key(),
                value: CborValue::Bool(true),
                mutation_id: None,
                inheritable: true,
            }),
            true,
        )
        .expect("replay malformed metadata");
    runtime
        .handle_event(
            Event::AgentReplayComplete(tau_proto::AgentReplayComplete {
                agent_id: agent_id.clone(),
                session_id: Some(
                    "session-1"
                        .parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                ),
                error: None,
            }),
            false,
        )
        .expect("complete replay");

    loop {
        let message = rx.recv().expect("discovery snapshot");
        if matches!(
            message,
            HarnessInputMessage::Emit(ref emit)
                if matches!(emit.event.as_ref(),
                    Event::ExtensionAgentDiscoverySnapshotDeclared(_))
        ) {
            break;
        }
    }
    let first = rx.recv().expect("invalid context");
    assert!(matches!(
        first,
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::ExtAgentContextPublish(publish)
                if publish.key.as_ref() == "workdir"
                    && publish.value.0["status"] == "invalid")
                && !emit.persist
    ));
    let second = rx.recv().expect("ready");
    assert!(matches!(
        second,
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::ExtensionContextReady(_))
                && !emit.persist
    ));
    assert!(
        rx.try_recv().is_err(),
        "malformed replay must not synthesize default metadata"
    );
}

/// Invalid remembered metadata fails one user shell command without killing
/// the extension runtime needed for an absolute workdir repair.
#[test]
fn invalid_workdir_user_shell_failure_is_command_local() {
    let (tx, rx) = mpsc::channel();
    let runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
    let agent_id = tau_proto::AgentId::parse("agent-invalid-user-shell").expect("agent id");
    runtime.cwd_state.set_invalid(agent_id.clone());
    runtime
        .handle_ui_shell_command(tau_proto::UiShellCommand {
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            command_id: tau_proto::ShellCommandId::parse("command-1")
                .expect("test identifier must satisfy its grammar"),
            command: "pwd".to_owned(),
            include_in_context: false,
            target_agent_id: Some(agent_id),
        })
        .expect("command-local failure");
    let event = rx.recv().expect("terminal user shell failure");
    assert!(matches!(
        event,
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::ShellCommandFinishedReported(finished)
                if finished.command_id.as_str() == "command-1"
                    && finished.output.contains("invalid"))
    ));
    assert!(runtime.scheduler.is_some(), "runtime must remain usable");
}

/// User shell work must not use process fallback while durable workdir
/// replay is still establishing whether the instance key is present.
#[test]
fn user_shell_before_workdir_replay_fails_without_spawning() {
    let (tx, rx) = mpsc::channel();
    let runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
    let agent_id = tau_proto::AgentId::parse("agent-replay-pending-shell").expect("agent id");
    runtime.cwd_state.set_pending_ready(
        agent_id.clone(),
        "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        tau_proto::AgentInitializationId::parse("init-1").expect("test identifier must be valid"),
    );
    runtime
        .handle_ui_shell_command(tau_proto::UiShellCommand {
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            command_id: tau_proto::ShellCommandId::parse("command-pending")
                .expect("test identifier must satisfy its grammar"),
            command: "touch must-not-exist".to_owned(),
            include_in_context: false,
            target_agent_id: Some(agent_id),
        })
        .expect("command-local failure");
    let event = rx.recv().expect("terminal failure");
    assert!(matches!(
        event,
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::ShellCommandFinishedReported(finished)
                if finished.output.contains("replay is not complete"))
    ));
}

/// Runtime shutdown clears setters awaiting lifecycle completion; the
/// harness owns terminalizing calls when the extension/session ends.
#[test]
fn shutdown_clears_reserved_workdir_setter_without_extension_terminal() {
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
    let agent_id = tau_proto::AgentId::parse("agent-reserved-setter").expect("agent id");
    let invoke = tau_proto::ToolStarted {
        call_id: tau_proto::ToolCallId::new("reserved-setter"),
        tool_name: tau_proto::ToolName::new(crate::tools::WORKDIR_TOOL_NAME),
        arguments: CborValue::Map(Vec::new()),
        agent_id: agent_id.clone(),
        originator: tau_proto::PromptOriginator::User,
    };
    runtime
        .cwd_state
        .start_pending_workdir_result(agent_id, PathBuf::from("/tmp"), invoke, None)
        .expect("reserve setter");
    runtime.shutdown();
    assert!(rx.try_recv().is_err());
    assert!(
        runtime
            .cwd_state
            .take_pending_workdir_by_call(&tau_proto::ToolCallId::new("reserved-setter"))
            .is_none()
    );
}

/// The non-interleavable pre-emission state and unrelated commits cannot
/// consume a setter; only its matching canonical echo reaches the terminal
/// boundary.
#[test]
fn workdir_reservation_commit_phase_is_linearized() {
    let (tx, _rx) = mpsc::channel();
    let runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
    let agent_id = tau_proto::AgentId::parse("agent-linearized-setter").expect("agent id");
    let invoke = tau_proto::ToolStarted {
        call_id: tau_proto::ToolCallId::new("x".repeat(1024)),
        tool_name: tau_proto::ToolName::new(crate::tools::WORKDIR_TOOL_NAME),
        arguments: CborValue::Map(Vec::new()),
        agent_id: agent_id.clone(),
        originator: tau_proto::PromptOriginator::User,
    };
    let expected = PathBuf::from("/expected");
    runtime
        .cwd_state
        .start_pending_workdir_result(agent_id.clone(), expected, invoke.clone(), None)
        .expect("reserve");
    let mutation_id = runtime
        .cwd_state
        .pending_workdir_mutation_id(&agent_id, &invoke.call_id)
        .expect("mutation id");
    assert!(mutation_id.as_str().len() <= tau_proto::MAX_AGENT_METADATA_MUTATION_ID_BYTES);
    assert!(
        runtime
            .cwd_state
            .take_committed_pending_workdir_result(
                &agent_id,
                &PathBuf::from("/pre-emission"),
                Some(&mutation_id),
            )
            .is_none()
    );
    assert!(
        runtime
            .cwd_state
            .mark_pending_workdir_awaiting_echo(&agent_id, &invoke.call_id)
    );
    assert!(
        runtime
            .cwd_state
            .take_committed_pending_workdir_result(&agent_id, &PathBuf::from("/superseding"), None,)
            .is_none(),
        "unrelated commit must not consume the setter"
    );
    assert!(
        runtime
            .cwd_state
            .take_committed_pending_workdir_result(&agent_id, &PathBuf::from("/expected"), None,)
            .is_none(),
        "same-value external commit must not impersonate the setter echo"
    );
    let completed = runtime
        .cwd_state
        .take_committed_pending_workdir_result(
            &agent_id,
            &PathBuf::from("/expected"),
            Some(&mutation_id),
        )
        .expect("matching echo consumes setter");
    assert!(completed.matched_request);
}

/// Awaiting-echo cancellation stays attached to the transaction and emits
/// exactly one cancellation when its correlated commit arrives.
#[test]
fn awaiting_workdir_cancel_terminalizes_at_correlated_commit() {
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
    let agent_id = tau_proto::AgentId::parse("agent-cancel-setter").expect("agent id");
    let invoke = tau_proto::ToolStarted {
        call_id: tau_proto::ToolCallId::new("cancel-setter"),
        tool_name: tau_proto::ToolName::new(crate::tools::WORKDIR_TOOL_NAME),
        arguments: CborValue::Map(Vec::new()),
        agent_id: agent_id.clone(),
        originator: tau_proto::PromptOriginator::User,
    };
    runtime
        .cwd_state
        .start_pending_workdir_result(
            agent_id.clone(),
            PathBuf::from("/tmp"),
            invoke.clone(),
            None,
        )
        .expect("reserve");
    let mutation_id = runtime
        .cwd_state
        .pending_workdir_mutation_id(&agent_id, &invoke.call_id)
        .expect("mutation id");
    assert!(
        runtime
            .cwd_state
            .mark_pending_workdir_awaiting_echo(&agent_id, &invoke.call_id)
    );
    runtime.handle_tool_cancel_request(tau_proto::ToolCancelRequest {
        target_call_id: invoke.call_id.clone(),
    });
    runtime.handle_agent_metadata_set(
        tau_proto::AgentMetadataSet {
            agent_id,
            key: runtime.cwd_state.key(),
            value: CborValue::Text("/tmp".to_owned()),
            mutation_id: Some(mutation_id),
            inheritable: true,
        },
        false,
    );
    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        events
            .iter()
            .filter(|message| matches!(
                message,
                HarnessInputMessage::Emit(emit)
                    if !emit.persist
                        && matches!(emit.event.as_ref(), Event::ToolCancelledReported(cancelled)
                        if cancelled.call_id.as_str() == "cancel-setter")
            ))
            .count(),
        1
    );
}

/// Ensures a session-level shutdown cleans shell-owned state without
/// dropping the scheduler needed by a subsequent session in the same
/// extension process.
#[test]
fn session_shutdown_keeps_scheduler_for_later_sessions() {
    let (tx, _rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());

    runtime
        .handle_event(
            Event::SessionShutdown(tau_proto::SessionShutdown {
                session_id: "session-1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
            }),
            false,
        )
        .expect("session shutdown");

    assert!(runtime.scheduler.is_some());
}
