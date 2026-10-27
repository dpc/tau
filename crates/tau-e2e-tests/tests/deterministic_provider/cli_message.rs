//! Actual CLI processes against a disposable, keyless recipient and message
//! tool.

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::Stdio;
use std::sync::mpsc;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use super::*;

/// One-way stdin delivery and a one-shot reply traverse real CLI admission,
/// durable receive folding, provider projection, and the production message
/// tool.
#[test]
fn deterministic_cli_message_one_way_and_final_response() -> Result<(), Box<dyn std::error::Error>>
{
    const SESSION: &str = "tau-cli-message";
    const AGENT: &str = "cli-recipient";
    const INPUT: &str = "one-way\nliteral </tau_internal> payload";
    const RESPONSE: &str = "final response\nno appended newline";
    let call_id = tau_proto::ToolCallId::new("cli-final-response");
    let scenario = ScenarioV2::new(
        "cli-message",
        vec![ScenarioLaneV2 {
            ctx_id: "seed".to_owned(),
            actions: vec![
                ScenarioActionV2::Text {
                    user_text: "seed".to_owned(),
                    response: "ready".to_owned(),
                },
                ScenarioActionV2::CliMessage {
                    message: INPUT.to_owned(),
                    response: None,
                    call_id: call_id.clone(),
                },
                ScenarioActionV2::CliMessage {
                    message: "session-address".to_owned(),
                    response: None,
                    call_id: tau_proto::ToolCallId::new("unused-session-call"),
                },
                ScenarioActionV2::CliMessage {
                    message: "question".to_owned(),
                    response: Some(RESPONSE.to_owned()),
                    call_id: call_id.clone(),
                },
                ScenarioActionV2::CliResponseResult { call_id },
                ScenarioActionV2::CliMessage {
                    message: "no tool response".to_owned(),
                    response: None,
                    call_id: tau_proto::ToolCallId::new("unused-disconnect-call"),
                },
            ],
        }],
    );
    let fixture = DeterministicFixture::new_session_message(
        "deterministic_cli_message_one_way_and_final_response",
        &scenario,
        FAKE_PROVIDER,
    )?;
    let config_path = fixture.config_dir().join("harness.yaml");
    let mut config: serde_json::Value = serde_json::from_slice(&std::fs::read(&config_path)?)?;
    config["agents"]["id_template"] = serde_json::json!(AGENT);
    config["inter_session"] = serde_json::json!({"receiver": {"role": "deterministic-main"}});
    std::fs::write(config_path, serde_json::to_vec_pretty(&config)?)?;
    let socket = fixture
        .runtime_dir()
        .join("tau/harnesses/sockets")
        .join(blake3::hash(SESSION.as_bytes()).to_hex().to_string())
        .with_extension("sock");
    let daemon = spawn_daemon_inner_with_cli_override(
        &fixture,
        &socket,
        tau_harness::SessionLaunchStatus::New,
        SESSION,
        64,
    );
    let mut ui = connect_ui(&socket)?;
    ui.send(&HarnessInputMessage::emit(Event::UiCreateAgent(
        tau_proto::UiCreateAgent {
            request_id: "cli-message-seed".to_owned(),
            literal: false,
            session_id: SESSION.parse()?,
            role: "deterministic-main".to_owned(),
            model_override: None,
            effort_override: None,
            metadata: Vec::new(),
            initial_prompt: Some("seed".to_owned()),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("seed".to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )))?;
    wait_for_idle(&mut ui, AGENT)?;
    let output = cli(&fixture)
        .args([
            "message",
            &format!("{SESSION}/missing-agent"),
            "not delivered",
        ])
        .output()?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("message rejected"));

    let mut child = cli(&fixture)
        .args(["message", &format!("&{SESSION}/@{AGENT}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(INPUT.as_bytes())?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "message delivered\n"
    );
    wait_for_idle(&mut ui, AGENT)?;
    let output = cli(&fixture)
        .args(["message", &format!("&{SESSION}"), "session-address"])
        .output()?;
    assert!(
        output.status.success(),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    wait_for_idle(&mut ui, AGENT)?;

    let output = cli(&fixture)
        .args(["message", "--wait-response", AGENT, "question"])
        .output()?;
    assert!(
        output.status.success(),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, RESPONSE.as_bytes());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "message delivered\n"
    );
    wait_for_idle(&mut ui, AGENT)?;
    let mut waiting = cli(&fixture)
        .args([
            "message",
            "--wait-response",
            &format!("{SESSION}/{AGENT}"),
            "no tool response",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = waiting.stderr.take().expect("stderr");
    let (ack_tx, ack_rx) = mpsc::channel();
    let ack_reader = std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stderr).read_line(&mut line).map(|_| line);
        let _ = ack_tx.send(result);
    });
    let ack = ack_rx.recv_timeout(Duration::from_secs(5));
    if ack.is_err() {
        waiting.kill()?;
        waiting.wait()?;
    }
    ack_reader.join().expect("ACK reader");
    assert_eq!(ack??, "message delivered\n");
    wait_for_idle(&mut ui, AGENT)?;
    assert!(
        waiting.try_wait()?.is_none(),
        "assistant text must not become a CLI response"
    );
    kill(Pid::from_raw(waiting.id().try_into()?), Signal::SIGINT)?;
    let output = waiting.wait_with_output()?;
    use std::os::unix::process::ExitStatusExt as _;
    assert_eq!(output.status.signal(), Some(nix::libc::SIGINT));
    assert!(output.stdout.is_empty());
    ui.send(&HarnessInputMessage::UiShutdownRequest(
        tau_proto::UiShutdownRequest {},
    ))?;
    drop(ui);
    daemon.finish()?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Each command gets an isolated runtime root and no ambient provider
/// credentials.
fn cli(fixture: &DeterministicFixture) -> Command {
    let mut command = Command::new(E2E_CLI);
    command
        .env_clear()
        .envs(tau_test_support::llvm_profile_environment())
        .env("HOME", fixture.root().join("home"))
        .env("XDG_CONFIG_HOME", fixture.root().join("xdg-config"))
        .env("XDG_STATE_HOME", fixture.root().join("xdg-state"))
        .env("XDG_CACHE_HOME", fixture.root().join("xdg-cache"))
        .env("XDG_RUNTIME_DIR", fixture.runtime_dir())
        .env("LANG", "C.UTF-8");
    command
}

/// Serialize requests behind a completed provider turn, not an arbitrary sleep.
fn wait_for_idle(peer: &mut SocketPeer, agent: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut finished = false;
    loop {
        match recv_event(peer)? {
            Event::ProviderResponseFinished(event)
                if event.agent_id.as_str() == agent
                    && event.stop_reason == tau_proto::ProviderStopReason::EndTurn =>
            {
                finished = true
            }
            Event::AgentStatsUpdated(stats)
                if finished
                    && stats.agent_id.as_str() == agent
                    && stats.runtime_state == AgentRuntimeState::Idle
                    && stats.tools.in_flight == 0 =>
            {
                return Ok(());
            }
            _ => {}
        }
    }
}
