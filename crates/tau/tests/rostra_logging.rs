//! End-to-end coverage for the bundled Rostra component's local-commit log.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rostra_core::id::RostraIdSecretKey;
use tau_proto::{
    Event, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter,
    PromptOriginator, ToolStarted,
};

/// The bundled subprocess honors the documented command, reaches the Rostra
/// target, and excludes post content and identity material from the
/// post-commit diagnostic.
#[test]
fn rostra_debug_command_reaches_the_bundled_rostra_target() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let identity = secret.id().to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tau"))
        .arg("component")
        .arg("ext-rostra")
        .env("TAU_LOG", "tau_ext_rostra=debug,rostra=debug,warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bundled Rostra component");
    let stdout = child.stdout.take().expect("component stdout");
    let stderr = child.stderr.take().expect("component stderr");
    let (result_tx, result_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut reader = HarnessInputReader::new(stdout);
        while let Ok(Some(message)) = reader.read_message() {
            if matches!(
                message,
                HarnessInputMessage::Emit(emit)
                    if matches!(*emit.event, Event::ToolResultReported(_))
            ) {
                let _ = result_tx.send(());
                return;
            }
        }
    });
    let stderr_reader = thread::spawn(move || {
        let mut stderr = stderr;
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });

    let mut input = HarnessOutputWriter::new(child.stdin.take().expect("component stdin"));
    input
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "identity_mnemonic_secret": "rostra_identity_mnemonic",
            })),
            instance_name: tau_proto::ExtensionName::parse("std-rostra").expect("extension name"),
            state_dir: Some(temporary.path().join("state")),
            secrets: BTreeMap::from([(
                "rostra_identity_mnemonic".to_owned(),
                tau_proto::SecretValue::new(secret.to_string()),
            )]),
            settings_files: Default::default(),
        }))
        .expect("configure Rostra");
    input
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
            ToolStarted {
                invocation_policy: tau_proto::ToolInvocationPolicy::default(),
                call_id: "subprocess-log-canary".into(),
                tool_name: tau_proto::ToolName::new("rostra_post"),
                arguments: tau_proto::json_to_cbor(&serde_json::json!({
                    "body": "subprocess private body canary",
                    "persona_tags": ["subprocess-private-tag"],
                })),
                agent_id: tau_proto::AgentId::parse("agent").expect("agent ID"),
                originator: PromptOriginator::User,
            },
        )))
        .expect("start post");
    input.flush().expect("flush component input");

    assert!(
        result_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
        "Rostra component did not report its locally stored post"
    );
    drop(input);
    let status = child.wait().expect("wait for component");
    reader.join().expect("component stdout reader");
    let stderr = String::from_utf8(stderr_reader.join().expect("component stderr reader"))
        .expect("component stderr is UTF-8");

    assert!(status.success());
    assert_eq!(stderr.matches("local_commit").count(), 2);
    assert!(stderr.contains("rostra"));
    let local_commit = stderr
        .lines()
        .find(|line| line.contains("call_id=subprocess-log-canary"))
        .expect("post-commit Rostra log record");
    let default_info = stderr
        .lines()
        .find(|line| line.contains("local_state=\"stored\""))
        .expect("default-info local commit record");
    assert!(!default_info.contains("call_id"));
    assert!(!default_info.contains("event_id"));
    let event_id = local_commit
        .split_whitespace()
        .find_map(|field| field.strip_prefix("event_id="))
        .expect("short event ID field");
    assert_eq!(event_id.chars().count(), 12);
    assert!(!local_commit.contains("subprocess private body canary"));
    assert!(!local_commit.contains("subprocess-private-tag"));
    assert!(!local_commit.contains(&identity));
    assert!(!local_commit.contains(&secret.to_string()));
}
