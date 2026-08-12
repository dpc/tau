use std::time as path_std_time;

use super::super::DiscoverySourcePolicy;
use super::*;

/// Ensures correlated delegate completion removes lifecycle state for queued
/// work that scheduler ownership drops, so late cancellation cannot report.
#[test]
fn start_agent_result_removes_queued_tool_lifecycle() {
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
    runtime.scheduler = Some(WorkScheduler::new(crate::scheduler::SchedulerConfig {
        control_workers: 0,
        user_workers: 0,
        cheap_workers: 0,
        general_workers: 0,
        ..Default::default()
    }));
    let agent_id = tau_proto::AgentId::parse("agent-delegate-result").expect("agent id");
    let cwd = std::env::current_dir().expect("current dir");
    runtime.cwd_state.set(agent_id.clone(), cwd.clone());
    runtime
        .start_agent_owners
        .insert("delegate-query".to_owned(), agent_id.clone());
    let call_id = tau_proto::ToolCallId::new("delegate-queued-read");
    schedule_runtime_tool(
        &runtime,
        call_id.clone(),
        crate::tools::READ_TOOL_NAME,
        CborValue::Map(vec![(
            CborValue::Text("path".to_owned()),
            CborValue::Text(cwd.join("Cargo.toml").display().to_string()),
        )]),
        agent_id,
    );

    runtime.handle_start_agent_result(tau_proto::StartAgentResult {
        query_id: "delegate-query".to_owned(),
        text: "done".to_owned(),
        error: None,
    });
    runtime.handle_tool_cancel_request(tau_proto::ToolCancelRequest {
        target_call_id: call_id,
    });

    assert!(
        rx.recv_timeout(path_std_time::Duration::from_millis(50))
            .is_err(),
        "removed queued work must not emit a late cancellation"
    );
}

/// Ensures disabling directory locking while an automatic mutation waits emits
/// one cancellation terminal rather than silently dropping the call.
#[test]
fn config_disable_reports_waiting_automatic_call_cancelled_once() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let path = tempdir.path().join("replace.txt");
    std::fs::write(&path, "old\n").expect("write fixture");
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
    configure_runtime_dir_lock(&mut runtime, true);
    while rx.try_recv().is_ok() {}
    let blocker = runtime
        .lock_manager
        .acquire_auto(
            tau_proto::ToolCallId::new("config-disable-blocker"),
            tau_proto::AgentId::parse("agent-blocker").expect("agent id"),
            vec![tempdir.path().canonicalize().expect("canonical tempdir")],
            || {},
        )
        .expect("blocking automatic lock");
    let agent_id = tau_proto::AgentId::parse("agent-config-waiter").expect("agent id");
    runtime
        .cwd_state
        .set(agent_id.clone(), tempdir.path().to_path_buf());
    let call_id = tau_proto::ToolCallId::new("config-disable-waiter");
    schedule_runtime_tool(
        &runtime,
        call_id.clone(),
        crate::tools::REPLACE_TOOL_NAME,
        CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(path.display().to_string()),
            ),
            (
                CborValue::Text("edits".to_owned()),
                CborValue::Array(vec![CborValue::Map(vec![
                    (
                        CborValue::Text("oldText".to_owned()),
                        CborValue::Text("old".to_owned()),
                    ),
                    (
                        CborValue::Text("newText".to_owned()),
                        CborValue::Text("new".to_owned()),
                    ),
                ])]),
            ),
        ]),
        agent_id,
    );
    wait_for_tool_progress(&rx, &call_id);

    configure_runtime_dir_lock(&mut runtime, false);
    assert_cancelled_report(&rx, &call_id);
    assert_no_second_terminal_for(&rx, &call_id);
    assert_eq!(std::fs::read_to_string(path).expect("fixture"), "old\n");
    drop(blocker);
    runtime.final_shutdown();
}

/// Ensures cancellation of a direct `dir_lock(update)` waiter still removes the
/// lock-manager waiter after the call has crossed its direct-dispatch boundary.
#[test]
fn tool_cancel_request_cancels_waiting_direct_dir_lock_once() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let dir = tempdir.path().canonicalize().expect("canonical tempdir");
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
    configure_runtime_dir_lock(&mut runtime, true);
    while rx.try_recv().is_ok() {}
    let blocker_id = tau_proto::AgentId::parse("agent-manual-blocker").expect("agent id");
    runtime
        .lock_manager
        .acquire_manual(
            tau_proto::ToolCallId::new("manual-blocker"),
            blocker_id.clone(),
            dir.clone(),
            || {},
        )
        .expect("blocking manual lock");
    let waiter_id = tau_proto::AgentId::parse("agent-manual-waiter").expect("agent id");
    runtime.cwd_state.set(waiter_id.clone(), dir.clone());
    let call_id = tau_proto::ToolCallId::new("direct-lock-waiter");
    schedule_runtime_tool(
        &runtime,
        call_id.clone(),
        crate::dir_lock::DIR_LOCK_TOOL_NAME,
        CborValue::Map(vec![
            (
                CborValue::Text("command".to_owned()),
                CborValue::Text("update".to_owned()),
            ),
            (
                CborValue::Text("directory".to_owned()),
                CborValue::Text(dir.display().to_string()),
            ),
        ]),
        waiter_id,
    );
    wait_for_tool_progress(&rx, &call_id);

    runtime.handle_tool_cancel_request(tau_proto::ToolCancelRequest {
        target_call_id: call_id.clone(),
    });
    assert_cancelled_report(&rx, &call_id);
    assert_no_second_terminal_for(&rx, &call_id);
    runtime.lock_manager.release_agent(&blocker_id);
    runtime.final_shutdown();
}

/// Ensures cancellation before a direct `dir_lock(update)` waiter registers is
/// bridged into registration and cannot later acquire the released lock.
#[test]
fn tool_cancel_request_bridges_direct_dir_lock_waiter_registration() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let dir = tempdir.path().canonicalize().expect("canonical tempdir");
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
    configure_runtime_dir_lock(&mut runtime, true);
    while rx.try_recv().is_ok() {}
    let blocker_id = tau_proto::AgentId::parse("agent-registration-blocker").expect("agent id");
    runtime
        .lock_manager
        .acquire_manual(
            tau_proto::ToolCallId::new("registration-blocker"),
            blocker_id.clone(),
            dir.clone(),
            || {},
        )
        .expect("blocking manual lock");
    let (reached_tx, reached_rx) = mpsc::sync_channel(0);
    let (resume_tx, resume_rx) = mpsc::channel();
    runtime
        .cancellation
        .lifecycles
        .pause_before_lock_waiter_registration(reached_tx, resume_rx);
    let waiter_id = tau_proto::AgentId::parse("agent-registration-waiter").expect("agent id");
    runtime.cwd_state.set(waiter_id.clone(), dir.clone());
    let call_id = tau_proto::ToolCallId::new("direct-lock-registration-gap");
    schedule_runtime_tool(
        &runtime,
        call_id.clone(),
        crate::dir_lock::DIR_LOCK_TOOL_NAME,
        CborValue::Map(vec![
            (
                CborValue::Text("command".to_owned()),
                CborValue::Text("update".to_owned()),
            ),
            (
                CborValue::Text("directory".to_owned()),
                CborValue::Text(dir.display().to_string()),
            ),
        ]),
        waiter_id,
    );
    reached_rx
        .recv_timeout(path_std_time::Duration::from_secs(1))
        .expect("direct lock reached pre-registration handoff");

    runtime.handle_tool_cancel_request(tau_proto::ToolCancelRequest {
        target_call_id: call_id.clone(),
    });
    resume_tx.send(()).expect("resume waiter registration");
    assert_cancelled_report(&rx, &call_id);
    assert_no_second_terminal_for(&rx, &call_id);
    runtime.lock_manager.release_agent(&blocker_id);
    runtime
        .lock_manager
        .acquire_manual(
            tau_proto::ToolCallId::new("post-cancel-lock"),
            tau_proto::AgentId::parse("agent-post-cancel").expect("agent id"),
            dir,
            || {},
        )
        .expect("cancelled waiter did not acquire lock");
    runtime.final_shutdown();
}

/// Ensures production cancellation routing preserves a request that arrives
/// after effect start but before the shell cancellation sender is registered.
#[test]
fn tool_cancel_request_survives_active_sender_registration_handoff() {
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
    let (call_id, reached_rx, resume_tx) =
        schedule_shell_paused_before_active_registration(&mut runtime, "cancel-handoff");

    reached_rx
        .recv_timeout(path_std_time::Duration::from_secs(1))
        .expect("shell reached sender-registration handoff");
    runtime.handle_tool_cancel_request(tau_proto::ToolCancelRequest {
        target_call_id: call_id.clone(),
    });
    resume_tx.send(()).expect("resume shell registration");

    assert_cancelled_report(&rx, &call_id);
    runtime.final_shutdown();
}

/// Ensures production cancellation routing also bridges effect start to the
/// separately implemented search cancellation sender registration.
#[test]
fn tool_cancel_request_survives_search_sender_registration_handoff() {
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
    let cwd = std::env::current_dir().expect("current dir");
    let (call_id, reached_rx, resume_tx) = schedule_tool_paused_before_active_registration(
        &mut runtime,
        "search-cancel-handoff",
        crate::tools::FIND_TOOL_NAME,
        CborValue::Map(vec![
            (
                CborValue::Text("pattern".to_owned()),
                CborValue::Text("**/*".to_owned()),
            ),
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(cwd.display().to_string()),
            ),
        ]),
    );

    reached_rx
        .recv_timeout(path_std_time::Duration::from_secs(1))
        .expect("find reached sender-registration handoff");
    runtime.handle_tool_cancel_request(tau_proto::ToolCancelRequest {
        target_call_id: call_id.clone(),
    });
    resume_tx.send(()).expect("resume find registration");

    assert_cancelled_report(&rx, &call_id);
    runtime.final_shutdown();
}

/// Ensures shutdown records cancellation in the lifecycle before snapshotting
/// active senders, so a shell delayed before registration cannot escape
/// teardown.
#[test]
fn shutdown_survives_active_sender_registration_handoff() {
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
    let (call_id, reached_rx, resume_tx) =
        schedule_shell_paused_before_active_registration(&mut runtime, "shutdown-handoff");

    reached_rx
        .recv_timeout(path_std_time::Duration::from_secs(1))
        .expect("shell reached sender-registration handoff");
    runtime.shutdown();
    resume_tx.send(()).expect("resume shell registration");
    runtime.final_shutdown();

    assert_cancelled_report(&rx, &call_id);
}

/// Schedule one shell call and pause it after effect start but before active
/// cancellation sender registration.
fn schedule_shell_paused_before_active_registration(
    runtime: &mut ShellRuntime,
    call_id: &str,
) -> (tau_proto::ToolCallId, mpsc::Receiver<()>, mpsc::Sender<()>) {
    schedule_tool_paused_before_active_registration(
        runtime,
        call_id,
        crate::tools::SHELL_TOOL_NAME,
        CborValue::Map(vec![(
            CborValue::Text("command".to_owned()),
            CborValue::Text("sleep 30".to_owned()),
        )]),
    )
}

/// Schedule one cancellable tool and pause it immediately before active sender
/// registration.
fn schedule_tool_paused_before_active_registration(
    runtime: &mut ShellRuntime,
    call_id: &str,
    tool_name: &str,
    arguments: CborValue,
) -> (tau_proto::ToolCallId, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let agent_id = tau_proto::AgentId::parse("agent-active-handoff").expect("agent id");
    runtime.cwd_state.set(
        agent_id.clone(),
        std::env::current_dir().expect("current dir"),
    );
    let (reached_tx, reached_rx) = mpsc::sync_channel(0);
    let (resume_tx, resume_rx) = mpsc::channel();
    runtime
        .cancellation
        .lifecycles
        .pause_before_active_registration(reached_tx, resume_rx);
    let call_id = tau_proto::ToolCallId::new(call_id);
    let invoke = tau_proto::ToolStarted {
        call_id: call_id.clone(),
        tool_name: tau_proto::ToolName::new(tool_name),
        arguments,
        agent_id,
        originator: tau_proto::PromptOriginator::User,
    };
    let local_tool_name = invoke.tool_name.clone();
    runtime
        .handle_tool_started(invoke, &local_tool_name, false)
        .expect("schedule shell");
    (call_id, reached_rx, resume_tx)
}

/// Schedule one model tool through the production runtime admission path.
fn schedule_runtime_tool(
    runtime: &ShellRuntime,
    call_id: tau_proto::ToolCallId,
    tool_name: &str,
    arguments: CborValue,
    agent_id: tau_proto::AgentId,
) {
    let invoke = tau_proto::ToolStarted {
        call_id,
        tool_name: tau_proto::ToolName::new(tool_name),
        arguments,
        agent_id,
        originator: tau_proto::PromptOriginator::User,
    };
    let local_tool_name = invoke.tool_name.clone();
    runtime
        .handle_tool_started(invoke, &local_tool_name, false)
        .expect("schedule runtime tool");
}

/// Apply one directory-lock enablement state through production configuration.
fn configure_runtime_dir_lock(runtime: &mut ShellRuntime, enable: bool) {
    let mut config = ExtConfig::default();
    config.dir_lock.enable = enable;
    runtime
        .apply_config(
            tau_proto::ExtensionName::parse("core-shell").expect("extension name"),
            None,
            config,
        )
        .expect("configure dir lock");
}

/// Wait until one tool reports that it is blocked on directory-lock
/// acquisition.
fn wait_for_tool_progress(
    rx: &mpsc::Receiver<HarnessInputMessage>,
    call_id: &tau_proto::ToolCallId,
) {
    loop {
        let HarnessInputMessage::Emit(emit) = rx
            .recv_timeout(path_std_time::Duration::from_secs(1))
            .expect("tool waiting progress")
        else {
            continue;
        };
        if let Event::ToolProgressReported(progress) = *emit.event
            && &progress.call_id == call_id
        {
            return;
        }
    }
}

/// Wait through progress events for the cancellation terminal of one call.
fn assert_cancelled_report(
    rx: &mpsc::Receiver<HarnessInputMessage>,
    call_id: &tau_proto::ToolCallId,
) {
    loop {
        let HarnessInputMessage::Emit(emit) = rx
            .recv_timeout(path_std_time::Duration::from_secs(2))
            .expect("shell cancellation report")
        else {
            continue;
        };
        if let Event::ToolCancelledReported(cancelled) = *emit.event {
            assert_eq!(&cancelled.call_id, call_id);
            return;
        }
    }
}

/// Assert that no later terminal report exists for one call.
fn assert_no_second_terminal_for(
    rx: &mpsc::Receiver<HarnessInputMessage>,
    call_id: &tau_proto::ToolCallId,
) {
    while let Ok(message) = rx.recv_timeout(path_std_time::Duration::from_millis(50)) {
        let HarnessInputMessage::Emit(emit) = message else {
            continue;
        };
        let terminal_call_id = match emit.event.as_ref() {
            Event::ToolResultReported(event) => Some(&event.call_id),
            Event::ToolErrorReported(event) => Some(&event.call_id),
            Event::ToolCancelledReported(event) => Some(&event.call_id),
            _ => None,
        };
        assert_ne!(
            terminal_call_id,
            Some(call_id),
            "call emitted more than one terminal"
        );
    }
}

/// Ensures ToolCancelRequest reaches already-running cancellable tool
/// calls, not just queued scheduler work or shell-only registry
/// entries.
#[test]
fn tool_cancel_request_signals_registered_running_call() {
    let (tx, _rx) = mpsc::channel();
    let runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
    let call_id = tau_proto::ToolCallId::new("running-find");
    let (cancel_tx, cancel_rx) = mpsc::channel();
    let lifecycle = runtime.cancellation.lifecycles.admit(
        call_id.clone(),
        tau_proto::ToolName::new("find"),
        tau_proto::AgentId::parse("agent-a").expect("agent id"),
        runtime.tx.clone(),
    );
    assert!(lifecycle.start_effect());
    runtime
        .cancellation
        .running_calls
        .lock()
        .expect("running call registry")
        .insert(call_id.clone(), cancel_tx);

    runtime.handle_tool_cancel_request(tau_proto::ToolCancelRequest {
        target_call_id: call_id,
    });

    cancel_rx
        .recv_timeout(path_std_time::Duration::from_millis(100))
        .expect("running call cancel signal");
}

/// Ensures runtime shutdown signals registered running cancellable tool
/// calls before scheduler drop waits for worker jobs to exit.
#[test]
fn shutdown_signals_registered_running_call() {
    let (tx, _rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
    let call_id = tau_proto::ToolCallId::new("running-grep");
    let (cancel_tx, cancel_rx) = mpsc::channel();
    runtime
        .cancellation
        .running_calls
        .lock()
        .expect("running call registry")
        .insert(call_id, cancel_tx);

    runtime.shutdown();

    cancel_rx
        .recv_timeout(path_std_time::Duration::from_millis(100))
        .expect("shutdown cancel signal");
}

/// Ensures replayed cwd metadata is folded for later boundary-approved
/// context readiness without emitting replay-time side effects.
#[test]
fn replayed_cwd_metadata_folds_without_emitting_until_live_agent_load() {
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
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
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
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
    let runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
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
    let runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
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
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
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
    let runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
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
            .committed_pending_workdir_result(
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
            .committed_pending_workdir_result(&agent_id, &PathBuf::from("/superseding"), None,)
            .is_none(),
        "unrelated commit must not consume the setter"
    );
    assert!(
        runtime
            .cwd_state
            .committed_pending_workdir_result(&agent_id, &PathBuf::from("/expected"), None,)
            .is_none(),
        "same-value external commit must not impersonate the setter echo"
    );
    let completed = runtime
        .cwd_state
        .committed_pending_workdir_result(
            &agent_id,
            &PathBuf::from("/expected"),
            Some(&mutation_id),
        )
        .expect("matching echo snapshots retained setter");
    assert!(completed.matched_request);
    assert!(
        runtime
            .cwd_state
            .take_pending_workdir_by_call(&invoke.call_id)
            .is_some(),
        "simulated successful terminal publication releases the reservation"
    );
}

/// Awaiting-echo cancellation stays attached to the transaction and emits
/// exactly one cancellation when its correlated commit arrives.
#[test]
fn awaiting_workdir_cancel_terminalizes_at_correlated_commit() {
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
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
    runtime
        .handle_agent_metadata_set(
            tau_proto::AgentMetadataSet {
                agent_id,
                key: runtime.cwd_state.key(),
                value: CborValue::Text("/tmp".to_owned()),
                mutation_id: Some(mutation_id),
                inheritable: true,
            },
            false,
        )
        .expect("metadata commit");
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
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );

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

/// Proves rapid per-agent initialization publishes each mandatory transaction
/// once and in snapshot, context, ready order.
#[test]
fn rapid_agent_initialization_publishes_complete_ordered_transactions() {
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::EmptyFixture,
    );
    let session_id = tau_proto::SessionId::parse("session-rapid").expect("session id");

    for index in 0..4 {
        let agent_id = tau_proto::AgentId::parse(format!("agent-{index}")).expect("agent id");
        runtime
            .cwd_state
            .set_metadata_text(agent_id.clone(), PathBuf::from("/tmp"));
        runtime
            .handle_event(
                Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                    agent_initialization_id: tau_proto::AgentInitializationId::parse(format!(
                        "init-{index}"
                    ))
                    .expect("initialization id"),
                    session_id: session_id.clone(),
                    agent_id: agent_id.clone(),
                    ephemeral: false,
                }),
                false,
            )
            .expect("agent load");
        runtime
            .handle_event(
                Event::AgentReplayComplete(tau_proto::AgentReplayComplete {
                    agent_id,
                    session_id: Some(session_id.clone()),
                    error: None,
                }),
                false,
            )
            .expect("replay completion");
    }

    let events = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|message| match message {
            HarnessInputMessage::Emit(emit) => Some(*emit.event),
            _ => None,
        })
        .collect::<Vec<_>>();
    for index in 0..4 {
        let agent_id = format!("agent-{index}");
        let sequence = events
            .iter()
            .filter_map(|event| match event {
                Event::ExtensionAgentDiscoverySnapshotDeclared(event)
                    if event.agent_id.as_str() == agent_id =>
                {
                    Some("snapshot")
                }
                Event::ExtAgentContextPublish(event) if event.agent_id.as_str() == agent_id => {
                    Some("context")
                }
                Event::ExtensionContextReady(event) if event.agent_id.as_str() == agent_id => {
                    Some("ready")
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(sequence, ["snapshot", "context", "ready"]);
    }
}

/// Proves a mandatory initialization write failure escapes event dispatch so
/// the manual extension loop can terminate the connection and release waiters.
#[test]
fn initialization_output_failure_fails_event_dispatch() {
    let (tx, rx) = mpsc::channel();
    drop(rx);
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::EmptyFixture,
    );

    let error = runtime
        .handle_event(
            Event::SessionStarted(tau_proto::SessionStarted {
                session_id: tau_proto::SessionId::parse("session-failure").expect("session id"),
                reason: tau_proto::SessionStartReason::New,
            }),
            false,
        )
        .expect_err("mandatory output failure must fail dispatch");
    assert!(matches!(error, tau_client::ClientError::WriterClosed));
}
