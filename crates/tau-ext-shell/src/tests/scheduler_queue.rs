//! Tests for scheduler queue behavior.

use super::*;

#[test]
fn disconnect_cancels_active_user_shell_command_before_scheduler_join() {
    let (mut reader, mut writer, done_rx) = spawn_extension_with_exit();
    drain_startup(&mut reader);

    writer
        .write_event(&ui_shell_command(
            "ui-sleep-disconnect",
            "printf started; sleep 30",
        ))
        .expect("ui shell");
    writer.flush().expect("flush ui shell");
    wait_for_user_shell_progress(&mut reader, "ui-sleep-disconnect", "started");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush disconnect");

    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("extension should exit after cancelling UI shell command")
        .expect("extension should exit cleanly");
}

#[test]
fn session_shutdown_cancels_active_user_shell() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::SessionStarted(tau_proto::SessionStarted {
            session_id: "session-1".parse().expect("session id"),
            reason: tau_proto::SessionStartReason::Initial,
        }))
        .expect("session start");
    writer
        .write_event(&ui_shell_command(
            "ui-sleep-session",
            "printf started; sleep 30",
        ))
        .expect("ui shell");
    writer.flush().expect("flush ui shell");
    wait_for_user_shell_progress(&mut reader, "ui-sleep-session", "started");

    writer
        .write_event(&Event::SessionShutdown(tau_proto::SessionShutdown {
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
        }))
        .expect("session shutdown");
    writer.flush().expect("flush session shutdown");

    let cancelled = wait_for_user_shell_finished(&mut reader, "ui-sleep-session");
    assert!(
        cancelled.cancelled,
        "session shutdown should cancel running UI shell command: {cancelled:?}"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush disconnect");
}

/// The real production detached FIFO may be full when a successful model tool
/// completes; checked output must drain earlier observations and publish once.
#[test]
fn saturated_production_fifo_preserves_model_result() {
    let frames = run_after_production_fifo_saturation(
        vec![tool_started(
            "call-saturated-result",
            ECHO_TOOL_NAME,
            cbor_text_map(vec![("text", "settled")]),
            "main",
        )],
        false,
    );
    assert_eq!(
        count_reported_terminal(
            &frames,
            "call-saturated-result",
            EventName::TOOL_RESULT_REPORTED
        ),
        1
    );
}

/// The real production detached FIFO may be full when a failing model tool
/// completes; checked output must publish its sole error once.
#[test]
fn saturated_production_fifo_preserves_model_error() {
    let frames = run_after_production_fifo_saturation(
        vec![tool_started(
            "call-saturated-error",
            READ_TOOL_NAME,
            cbor_text_map(vec![("path", "/definitely/missing/tau-ext-shell")]),
            "main",
        )],
        false,
    );
    assert_eq!(
        count_reported_terminal(
            &frames,
            "call-saturated-error",
            EventName::TOOL_ERROR_REPORTED
        ),
        1
    );
}

/// Ensures the dispatch path returns a clear bounded-backpressure ToolError
/// when scheduler admission rejects excess work.
#[test]
fn schedule_tool_started_reports_queue_full_error() {
    let (tx, _rx) = path_std_sync::mpsc::channel();
    let output = Output::channel(tx);
    let scheduler = WorkScheduler::new(crate::scheduler::SchedulerConfig {
        total_limit: 0,
        control_workers: 0,
        user_workers: 0,
        cheap_workers: 0,
        general_workers: 0,
        ..Default::default()
    });
    let Event::ToolStarted(invoke) = tool_started(
        "queue-full-read",
        READ_TOOL_NAME,
        cbor_text_map(vec![("path", "Cargo.toml")]),
        "agent-a",
    ) else {
        panic!("expected tool started");
    };
    let local_tool_name = invoke.tool_name.clone();

    let Err(error) = schedule_tool_started(
        (invoke, &local_tool_name),
        &scheduler,
        &output,
        ExtConfig::default(),
        DirLockManager::default(),
        ToolCancellationState::default(),
        CwdState::new(),
    ) else {
        panic!("queue-full call should be rejected");
    };
    let (returned, failure) = *error;

    assert_eq!(returned.call_id.as_str(), "queue-full-read");
    assert!(failure.message.contains("queue limit is 0"));
}
/// Cancellation remains the sole terminal when it races a sleeping shell call
/// immediately after actual detached FIFO exhaustion.
#[test]
fn saturated_production_fifo_preserves_model_cancellation() {
    let call_id = tau_proto::ToolCallId::new("call-saturated-cancel");
    let frames = run_after_production_fifo_saturation(
        vec![
            tool_started(
                call_id.as_str(),
                SHELL_TOOL_NAME,
                cbor_text_map(vec![("command", "sleep 5")]),
                "main",
            ),
            Event::ToolCancelRequest(ToolCancelRequest {
                target_call_id: call_id,
            }),
        ],
        false,
    );
    assert_eq!(
        count_reported_terminal(
            &frames,
            "call-saturated-cancel",
            EventName::TOOL_CANCELLED_REPORTED,
        ),
        1
    );
}

/// A user-shell completion queued after actual detached FIFO exhaustion must
/// retain include-in-context correlation and publish exactly once.
#[test]
fn saturated_production_fifo_preserves_user_shell_completion() {
    let frames = run_after_production_fifo_saturation(
        vec![ui_shell_command(
            "ui-saturated-completion",
            "printf settled",
        )],
        false,
    );
    let completions = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                Event::ShellCommandFinishedReported(event)
                    if event.command_id.as_str() == "ui-saturated-completion" =>
                {
                    Some(event)
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completions.len(), 1);
    assert!(completions[0].include_in_context);
    assert_eq!(completions[0].output, "settled");
}

/// Scheduler rejection after actual detached FIFO exhaustion still publishes
/// one include-in-context user-shell completion.
#[test]
fn saturated_production_fifo_preserves_rejected_user_shell_completion() {
    let Event::UiShellCommand(mut command) = ui_shell_command(
        "ui-saturated-rejection",
        "x".repeat(crate::scheduler::DEFAULT_QUEUED_BYTES_LIMIT + 1)
            .as_str(),
    ) else {
        unreachable!();
    };
    command.include_in_context = true;
    let frames = run_after_production_fifo_saturation(vec![Event::UiShellCommand(command)], false);
    let completions = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                Event::ShellCommandFinishedReported(event)
                    if event.command_id.as_str() == "ui-saturated-rejection" =>
                {
                    Some(event)
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completions.len(), 1);
    assert!(completions[0].include_in_context);
    assert!(completions[0].output.contains("queue"));
}

/// Ensures cancellation at the scheduler dequeue-to-dispatch handoff wins the
/// atomic pre-effect transition, so no file mutation or duplicate terminal can
/// occur.
#[test]
fn cancellation_after_dequeue_prevents_mutation_once() {
    let tempdir = TempDir::new().expect("tempdir");
    let edit_path = tempdir.path().join("dequeued-edit.txt");
    fs::write(&edit_path, "old\n").expect("initial file");
    let (tx, rx) = path_std_sync::mpsc::channel();
    let output = Output::channel(tx);
    let scheduler = WorkScheduler::new(crate::scheduler::SchedulerConfig {
        control_workers: 0,
        user_workers: 0,
        cheap_workers: 0,
        general_workers: 1,
        ..Default::default()
    });
    let lifecycles = ToolLifecycleRegistry::default();
    let (reached_tx, reached_rx) = path_std_sync::mpsc::sync_channel(0);
    let (resume_tx, resume_rx) = path_std_sync::mpsc::channel();
    lifecycles.pause_after_dequeue(reached_tx, resume_rx);
    let Event::ToolStarted(invoke) = tool_started(
        "dequeued-edit",
        EDIT_TOOL_NAME,
        edit_arguments(&edit_path, vec![line_edit(1, 2, "new\n")]),
        "agent-a",
    ) else {
        panic!("expected tool started");
    };
    let call_id = invoke.call_id.clone();
    let local_tool_name = invoke.tool_name.clone();

    schedule_tool_started(
        (invoke, &local_tool_name),
        &scheduler,
        &output,
        ExtConfig::default(),
        DirLockManager::default(),
        ToolCancellationState {
            lifecycles: lifecycles.clone(),
            ..Default::default()
        },
        CwdState::new(),
    )
    .expect("edit scheduled");
    reached_rx.recv().expect("worker reached dequeue handoff");
    assert_eq!(
        lifecycles.cancel(&call_id),
        Some(crate::tool_lifecycle::CancelOutcome::PreventedEffect)
    );
    resume_tx.send(()).expect("resume worker");
    drop(scheduler);

    assert_eq!(lifecycles.cancel(&call_id), None);
    assert_cancelled_terminal_once(&rx, &call_id);
    assert_eq!(fs::read_to_string(&edit_path).expect("file"), "old\n");
}
/// Ensures a model tool canceled while queued by the scheduler never reaches
/// the mutation implementation.
#[test]
fn schedule_tool_started_cancel_before_start_prevents_mutation() {
    let tempdir = TempDir::new().expect("tempdir");
    let edit_path = tempdir.path().join("queued-edit.txt");
    fs::write(&edit_path, "old\n").expect("initial file");
    let (tx, rx) = path_std_sync::mpsc::channel();
    let output = Output::channel(tx);
    let scheduler = WorkScheduler::new(crate::scheduler::SchedulerConfig {
        control_workers: 0,
        user_workers: 0,
        cheap_workers: 0,
        general_workers: 0,
        ..Default::default()
    });
    let Event::ToolStarted(invoke) = tool_started(
        "queued-edit",
        EDIT_TOOL_NAME,
        edit_arguments(&edit_path, vec![line_edit(1, 2, "new\n")]),
        "agent-a",
    ) else {
        panic!("expected tool started");
    };
    let call_id = invoke.call_id.clone();
    let local_tool_name = invoke.tool_name.clone();

    let lifecycles = ToolLifecycleRegistry::default();
    schedule_tool_started(
        (invoke, &local_tool_name),
        &scheduler,
        &output,
        ExtConfig::default(),
        DirLockManager::default(),
        ToolCancellationState {
            lifecycles: lifecycles.clone(),
            ..Default::default()
        },
        CwdState::new(),
    )
    .expect("edit queued");
    assert_eq!(
        lifecycles.cancel(&call_id),
        Some(crate::tool_lifecycle::CancelOutcome::PreventedEffect)
    );
    assert!(scheduler.cancel_queued_call(&call_id));

    let HarnessInputMessage::Emit(emit) = rx.recv().expect("cancel event") else {
        panic!("expected emit");
    };
    assert!(!emit.persist);
    let Event::ToolCancelledReported(cancelled) = *emit.event else {
        panic!("expected ToolCancelledReported");
    };
    assert_eq!(cancelled.call_id, call_id);
    assert_eq!(fs::read_to_string(&edit_path).expect("file"), "old\n");
}
