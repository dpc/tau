use std::fs;
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;

use tau_proto::{
    CborValue, ClientKind, Disconnect, Event, HarnessInputMessage, HarnessOutputMessage, Hello,
    PROTOCOL_VERSION, Ready, Subscribe, ToolRegister, ToolStarted, UnixMicros,
};
use tau_supervisor::{
    ExtensionCommand, ReceiveOutcome, StderrPolicy, SupervisedChild, SupervisionError,
};

const SECRET_ENV_SUBPROCESS: &str = "TAU_SUPERVISOR_SECRET_ENV_SUBPROCESS";
const STDERR_POLICY_SUBPROCESS: &str = "TAU_SUPERVISOR_STDERR_POLICY_SUBPROCESS";
const FLOOD_MESSAGE_COUNT: usize = 128;

fn test_command(args: &[&str]) -> ExtensionCommand {
    ExtensionCommand {
        name: "test-child".into(),
        program: PathBuf::from(env!("CARGO_BIN_EXE_tau-supervisor-test-child")),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        working_dir: None,
        stderr: StderrPolicy::Inherit,
    }
}

fn expect_message(child: &mut SupervisedChild, label: &str) -> HarnessInputMessage {
    match child
        .recv_timeout(Duration::from_secs(1))
        .unwrap_or_else(|error| panic!("{label} should decode: {error}"))
    {
        ReceiveOutcome::Message(message) => *message,
        ReceiveOutcome::Timeout => panic!("{label} should arrive before timeout"),
        ReceiveOutcome::Closed => panic!("{label} should arrive before stdout closes"),
    }
}

fn expect_child_startup(child: &mut SupervisedChild) -> ToolRegister {
    let hello = expect_message(child, "hello");
    assert_eq!(
        hello,
        HarnessInputMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "test-child".into(),
            client_kind: ClientKind::Tool,
        })
    );

    let subscribe = expect_message(child, "subscribe");
    assert_eq!(
        subscribe,
        HarnessInputMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![tau_proto::EventSelector::Exact(
                tau_proto::EventName::TOOL_STARTED,
            )],
        })
    );

    let register = expect_message(child, "register");
    let HarnessInputMessage::Emit(emit) = register else {
        panic!("expected tool register emit");
    };
    let Event::ToolRegister(register) = *emit.event else {
        panic!("expected tool register event");
    };

    let ready = expect_message(child, "ready");
    assert_eq!(
        ready,
        HarnessInputMessage::Ready(Ready {
            message: Some("ready".to_owned()),
        })
    );

    register
}

fn disconnect_child(child: &mut SupervisedChild, reason: &str) {
    child
        .send(&HarnessOutputMessage::Disconnect(Disconnect {
            reason: Some(reason.to_owned()),
        }))
        .expect("disconnect should be sent");
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    StdCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Ensures receive timeout is observable without treating the child as
/// disconnected.
#[test]
fn recv_timeout_reports_timeout_without_conflating_disconnect() {
    let mut child = SupervisedChild::spawn(test_command(&[])).expect("child should spawn");
    let _register = expect_child_startup(&mut child);

    assert_eq!(
        child
            .recv_timeout(Duration::from_millis(20))
            .expect("timeout should not be an error"),
        ReceiveOutcome::Timeout
    );

    disconnect_child(&mut child, "done");
    let _exit = child
        .wait_for_exit(Duration::from_secs(2))
        .expect("child should exit");
}

/// Ensures clean stdout EOF is reported separately from timeout and decode
/// failure.
#[test]
fn recv_timeout_reports_clean_stdout_close() {
    let mut child =
        SupervisedChild::spawn(test_command(&["--exit-immediately"])).expect("child should spawn");

    assert_eq!(
        child
            .recv_timeout(Duration::from_secs(1))
            .expect("clean close should not be an error"),
        ReceiveOutcome::Closed
    );
    let exit = child
        .wait_for_exit(Duration::from_secs(2))
        .expect("child should exit");
    assert_eq!(exit.exit_code(), Some(0));
}

/// Ensures the one-shot waiter notification is cached after the first
/// observation so later status checks report the same child exit.
#[test]
fn child_exit_observation_is_repeatable() {
    let mut child =
        SupervisedChild::spawn(test_command(&["--exit-immediately"])).expect("child should spawn");

    let first = child
        .wait_for_exit(Duration::from_secs(2))
        .expect("child should exit");
    let second = child
        .wait_for_exit(Duration::from_secs(2))
        .expect("cached exit should be returned");
    let third = child
        .try_wait()
        .expect("cached exit status should be readable")
        .expect("cached exit should be present");

    assert_eq!(first, second);
    assert_eq!(second, third);
}

/// Ensures truncated protocol data remains a decode error instead of a clean
/// close.
#[test]
fn recv_timeout_reports_partial_frame_as_decode_error() {
    let mut child =
        SupervisedChild::spawn(test_command(&["--partial-frame"])).expect("child should spawn");

    let error = child
        .recv_timeout(Duration::from_secs(1))
        .expect_err("partial frame should be a decode error");
    assert!(matches!(error, SupervisionError::Decode(_)));
    let _exit = child
        .wait_for_exit(Duration::from_secs(2))
        .expect("child should exit");
}

/// Ensures the stdout reader can drain a burst of child messages without loss.
#[test]
fn stdout_reader_handles_message_burst_without_loss() {
    let flood_message_count = FLOOD_MESSAGE_COUNT.to_string();
    let mut child =
        SupervisedChild::spawn(test_command(&["--flood", flood_message_count.as_str()]))
            .expect("child should spawn");

    for index in 0..FLOOD_MESSAGE_COUNT {
        assert_eq!(
            child
                .recv_timeout(Duration::from_secs(1))
                .expect("flood message should decode"),
            ReceiveOutcome::Message(Box::new(HarnessInputMessage::Ready(Ready {
                message: Some(index.to_string()),
            })))
        );
    }
    assert_eq!(
        child
            .recv_timeout(Duration::from_secs(1))
            .expect("clean close should decode"),
        ReceiveOutcome::Closed
    );
    let exit = child
        .wait_for_exit(Duration::from_secs(2))
        .expect("child should exit");
    assert_eq!(exit.exit_code(), Some(0));
}

/// Ensures the spawn policy applies the configured child working directory.
#[test]
fn spawn_uses_configured_working_dir() {
    let working_dir =
        std::env::temp_dir().join(format!("tau-supervisor-cwd-{}", std::process::id()));
    fs::create_dir_all(&working_dir).expect("working dir should be created");

    let mut command = test_command(&["--report-cwd"]);
    command.working_dir = Some(working_dir.clone());
    let mut child = SupervisedChild::spawn(command).expect("child should spawn");

    assert_eq!(
        child
            .recv_timeout(Duration::from_secs(1))
            .expect("cwd report should decode"),
        ReceiveOutcome::Message(Box::new(HarnessInputMessage::Ready(Ready {
            message: Some(working_dir.display().to_string()),
        })))
    );
    let exit = child
        .wait_for_exit(Duration::from_secs(2))
        .expect("child should exit");
    assert_eq!(exit.exit_code(), Some(0));
    fs::remove_dir_all(working_dir).expect("working dir should be removed");
}

/// Ensures relative program paths are rejected when a working directory is set.
#[test]
fn spawn_rejects_relative_program_with_working_dir() {
    let working_dir = std::env::temp_dir().join(format!(
        "tau-supervisor-relative-program-{}",
        std::process::id()
    ));
    fs::create_dir_all(&working_dir).expect("working dir should be created");

    let mut command = test_command(&[]);
    command.program = PathBuf::from("tau-supervisor-test-child");
    command.working_dir = Some(working_dir.clone());

    let error = match SupervisedChild::spawn(command) {
        Ok(_) => panic!("relative program with working dir should be rejected"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SupervisionError::RelativeProgramWithWorkingDir { .. }
    ));
    fs::remove_dir_all(working_dir).expect("working dir should be removed");
}

/// Ensures pre-spawn starting events intentionally omit the child pid.
#[test]
fn pre_spawn_starting_event_has_no_pid() {
    let command = test_command(&[]);

    assert_eq!(
        command.pre_spawn_starting_event(7.into()),
        Event::ExtensionStarting(tau_proto::ExtensionStarting {
            instance_id: 7.into(),
            extension_name: "test-child".into(),
            pid: None,
        })
    );
}

/// Ensures explicit hard termination can clean up a child that ignores protocol
/// shutdown.
#[cfg(target_os = "linux")]
#[test]
fn terminate_kills_long_running_child() {
    let mut child = SupervisedChild::spawn(test_command(&["--sleep"])).expect("child should spawn");

    let exit = child
        .terminate(Duration::from_secs(2))
        .expect("child should terminate");
    assert_ne!(exit.exit_code(), Some(0));
}

/// Ensures Drop performs best-effort direct child cleanup when callers forget
/// explicit termination.
#[cfg(target_os = "linux")]
#[test]
fn drop_kills_long_running_direct_child() {
    let pid = {
        let child = SupervisedChild::spawn(test_command(&["--sleep"])).expect("child should spawn");
        child.pid()
    };

    for _ in 0..200 {
        if !process_exists(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("dropped child process should exit");
}

/// Ensures the null stderr policy discards child stderr output.
#[test]
fn stderr_policy_null_discards_child_stderr() {
    if std::env::var_os(STDERR_POLICY_SUBPROCESS).is_some() {
        let mut command = test_command(&["--stderr-marker"]);
        command.stderr = StderrPolicy::Null;
        let mut child = SupervisedChild::spawn(command).expect("child should spawn");
        assert_eq!(
            child
                .recv_timeout(Duration::from_secs(1))
                .expect("stderr marker child should report readiness"),
            ReceiveOutcome::Message(Box::new(HarnessInputMessage::Ready(Ready {
                message: Some("stderr-written".to_owned()),
            })))
        );
        let exit = child
            .wait_for_exit(Duration::from_secs(2))
            .expect("child should exit");
        assert_eq!(exit.exit_code(), Some(0));
        return;
    }

    let output = StdCommand::new(std::env::current_exe().expect("test binary path"))
        .arg("--exact")
        .arg("stderr_policy_null_discards_child_stderr")
        .arg("--nocapture")
        .env(STDERR_POLICY_SUBPROCESS, "1")
        .output()
        .expect("stderr regression subprocess should run");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
}

/// Ensures supervised children do not inherit parent `TAU_SECRET_*` values.
#[test]
fn spawned_child_does_not_inherit_tau_secret_env() {
    if std::env::var_os(SECRET_ENV_SUBPROCESS).is_some() {
        let mut child = SupervisedChild::spawn(test_command(&["--report-secret-env"]))
            .expect("child should spawn");
        assert_eq!(
            child
                .recv_timeout(Duration::from_secs(1))
                .expect("env report should decode"),
            ReceiveOutcome::Message(Box::new(HarnessInputMessage::Ready(Ready {
                message: Some("absent".to_owned()),
            })))
        );
        let _exit = child
            .wait_for_exit(Duration::from_secs(2))
            .expect("child should exit");
        return;
    }

    let status = StdCommand::new(std::env::current_exe().expect("test binary path"))
        .arg("--exact")
        .arg("spawned_child_does_not_inherit_tau_secret_env")
        .arg("--nocapture")
        .env(SECRET_ENV_SUBPROCESS, "1")
        .env("TAU_SECRET_REGRESSION", "must-not-leak")
        .status()
        .expect("env regression subprocess should run");
    assert!(status.success());
}

/// Ensures the supervisor exchanges lifecycle and tool events over child stdio.
#[test]
fn supervised_child_exchanges_protocol_events_over_stdio() {
    let command = test_command(&[]);
    let mut child = SupervisedChild::spawn(command.clone()).expect("child should spawn");

    assert_eq!(child.command(), &command);
    assert_eq!(
        child.starting_event(42.into()),
        Event::ExtensionStarting(tau_proto::ExtensionStarting {
            instance_id: 42.into(),
            extension_name: "test-child".into(),
            pid: Some(child.pid()),
        })
    );

    let register = expect_child_startup(&mut child);
    assert_eq!(
        register,
        ToolRegister {
            tool: tau_proto::ToolSpec {
                name: tau_proto::ToolName::new("echo"),
                model_visible_name: None,
                description: Some("Echo test payloads".to_owned()),
                tool_type: tau_proto::ToolType::Function,
                parameters: None,
                format: None,
                tags: Vec::new(),
                enabled_by_default: true,
                background_support: None,
                examples: Vec::new(),
            },
            tool_group: None,
            prompt_fragment: None,
        }
    );
    assert_eq!(
        child.ready_event(42.into()),
        Event::ExtensionReady(tau_proto::ExtensionReady {
            instance_id: 42.into(),
            extension_name: "test-child".into(),
            pid: Some(child.pid()),
        })
    );

    child
        .send(&HarnessOutputMessage::deliver_replay(
            UnixMicros::new(7),
            Event::ToolStarted(ToolStarted {
                call_id: "call-replay".into(),
                tool_name: tau_proto::ToolName::new("echo"),
                arguments: CborValue::Text("replayed".to_owned()),
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                originator: tau_proto::PromptOriginator::User,
            }),
        ))
        .expect("replayed tool should be sent");
    child
        .send(&HarnessOutputMessage::deliver(Event::ToolStarted(
            ToolStarted {
                call_id: "call-1".into(),
                tool_name: tau_proto::ToolName::new("echo"),
                arguments: CborValue::Text("hello".to_owned()),
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                originator: tau_proto::PromptOriginator::User,
            },
        )))
        .expect("tool should be sent");
    let result = expect_message(&mut child, "tool result");
    assert_eq!(
        result,
        HarnessInputMessage::emit(Event::ToolResult(tau_proto::ToolResult {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("echo"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("hello".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }))
    );

    disconnect_child(&mut child, "done");
    let exit = child
        .wait_for_exit(Duration::from_secs(2))
        .expect("child should exit");
    assert_eq!(exit.exit_code(), Some(0));
    assert_eq!(
        child.exited_event(42.into(), &exit),
        Event::ExtensionExited(tau_proto::ExtensionExited {
            instance_id: 42.into(),
            extension_name: "test-child".into(),
            pid: Some(child.pid()),
            exit_code: exit.exit_code(),
            signal: exit.signal(),
        })
    );
}

/// Ensures a restarted child can emit the same tool registration after prior
/// exit.
#[test]
fn restarted_child_can_reregister_after_exit() {
    let command = test_command(&[]);

    for _ in 0..2 {
        let mut child = SupervisedChild::spawn(command.clone()).expect("child should spawn");
        let register = expect_child_startup(&mut child);
        assert_eq!(register.tool.name, tau_proto::ToolName::new("echo"));

        disconnect_child(&mut child, "restart");
        let exit = child
            .wait_for_exit(Duration::from_secs(2))
            .expect("child should exit");
        assert_eq!(exit.exit_code(), Some(0));
    }
}
