use super::*;

/// Builds one current user-shell command for bounded snapshot selection.
fn snapshot_shell(id: &str, command: String) -> tau_proto::UiShellCommand {
    tau_proto::UiShellCommand {
        session_id: tau_proto::SessionId::parse("snapshot-session").expect("session id"),
        command_id: tau_proto::ShellCommandId::parse(id).expect("command id"),
        command,
        include_in_context: true,
        target_agent_id: Some(crate::parse_agent_id("snapshot-agent")),
    }
}

/// Snapshot item selection is bounded and deterministic regardless of map
/// order.
#[test]
fn running_shell_snapshot_selects_first_128_public_ids() {
    let commands = (0..129)
        .rev()
        .map(|index| snapshot_shell(&format!("shell-{index:03}"), "x".to_owned()))
        .collect::<Vec<_>>();

    let (selected, omitted) = bounded_running_shell_snapshot(commands.iter());

    assert_eq!(selected.len(), 128);
    assert_eq!(omitted, 1);
    assert_eq!(selected[0].command_id.as_str(), "shell-000");
    assert_eq!(selected[127].command_id.as_str(), "shell-127");
}

/// An oversized earlier payload is skipped without starving a later small
/// route.
#[test]
fn running_shell_snapshot_skips_over_budget_route_and_continues() {
    let commands = [
        snapshot_shell("shell-a", "a".repeat(40 * 1024)),
        snapshot_shell("shell-b", "b".repeat(40 * 1024)),
        snapshot_shell("shell-c", "small".to_owned()),
    ];

    let (selected, omitted) = bounded_running_shell_snapshot(commands.iter());

    assert_eq!(omitted, 1);
    assert_eq!(
        selected
            .iter()
            .map(|command| command.command_id.as_str())
            .collect::<Vec<_>>(),
        ["shell-a", "shell-c"]
    );
}

/// Candidate scanning continues beyond the 128th public id when an earlier
/// oversized route cannot consume an accepted snapshot slot.
#[test]
fn running_shell_snapshot_counts_accepted_not_examined_routes() {
    let mut commands = (1..=127)
        .map(|index| snapshot_shell(&format!("shell-{index:03}"), "x".to_owned()))
        .collect::<Vec<_>>();
    commands.push(snapshot_shell("shell-000", "x".repeat(65 * 1024)));
    commands.push(snapshot_shell("shell-129", "last".to_owned()));

    let (selected, omitted) = bounded_running_shell_snapshot(commands.iter());

    assert_eq!(selected.len(), 128);
    assert_eq!(omitted, 1);
    assert_eq!(selected[0].command_id.as_str(), "shell-001");
    assert_eq!(selected[127].command_id.as_str(), "shell-129");
}

/// The aggregate budget admits an exactly 64 KiB CBOR payload and rejects the
/// first byte beyond it.
#[test]
fn running_shell_snapshot_enforces_exact_cbor_byte_boundary() {
    let probe = snapshot_shell("shell-boundary", "x".repeat(60 * 1024));
    let mut encoded = Vec::new();
    ciborium::into_writer(&probe, &mut encoded).expect("encode probe");
    let overhead = encoded.len() - probe.command.len();
    let exact = snapshot_shell(
        "shell-boundary",
        "x".repeat(RUNNING_SHELL_SNAPSHOT_MAX_BYTES - overhead),
    );
    let over = snapshot_shell(
        "shell-boundary",
        "x".repeat(RUNNING_SHELL_SNAPSHOT_MAX_BYTES - overhead + 1),
    );
    let mut exact_encoded = Vec::new();
    ciborium::into_writer(&exact, &mut exact_encoded).expect("encode exact fixture");
    let mut over_encoded = Vec::new();
    ciborium::into_writer(&over, &mut over_encoded).expect("encode over-budget fixture");

    assert_eq!(exact_encoded.len(), RUNNING_SHELL_SNAPSHOT_MAX_BYTES);
    assert_eq!(over_encoded.len(), RUNNING_SHELL_SNAPSHOT_MAX_BYTES + 1);

    let (selected, omitted) = bounded_running_shell_snapshot([&exact].into_iter());
    assert_eq!(selected, [exact]);
    assert_eq!(omitted, 0);
    let (selected, omitted) = bounded_running_shell_snapshot([&over].into_iter());
    assert!(selected.is_empty());
    assert_eq!(omitted, 1);
}

/// Accepted manual-compaction work is a durable user-visible lifecycle
/// fact, so late subscribers must receive it before a matching
/// transaction start.
#[test]
fn late_subscriber_replays_manual_compaction_acceptance() {
    let event = Event::AgentManualCompactionRequested(tau_proto::AgentManualCompactionRequested {
        request_id: tau_proto::CompactionRequestId::parse("cr-1-0").expect("request id"),
        caller_agent_id: crate::parse_agent_id("manager"),
        target_agent_id: crate::parse_agent_id("worker"),
        initiating_agent_prompt_id: "ap-manager-1"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        initiating_tool_call_id: "call-1".into(),
        initiating_tool_name: tau_proto::ManualCompactionTool::AgentCompact,
        visible_tool_name: tau_proto::ToolName::new("agent_compact"),
        requested_target_head: tau_proto::AgentHead::Root,
        target_generation: 0,
        model: "test/model".parse().expect("model id"),
        resume_inference: false,
    });

    assert!(should_replay_agent_event_to_late_subscriber(&event));
}

/// A completed context shell command is a self-contained final transcript fact,
/// while its running observations remain outside late-subscriber replay.
#[test]
fn late_subscriber_replays_only_completed_context_shell_fact() {
    let target = crate::parse_agent_id("worker");
    let finished = Event::ShellCommandFinished(tau_proto::ShellCommandFinished {
        command_id: tau_proto::ShellCommandId::parse("shell-replay").expect("command id"),
        session_id: tau_proto::SessionId::parse("session-replay").expect("session id"),
        command: "sh -c 'printf output; exit 7'".to_owned(),
        include_in_context: true,
        target_agent_id: Some(target.clone()),
        output: "output".to_owned(),
        exit_code: Some(7),
        cancelled: false,
    });

    assert!(should_replay_agent_event_to_late_subscriber(&finished));
    assert_eq!(project_agent_replay_event(finished.clone(), true), finished);
    assert!(!should_replay_agent_event_to_late_subscriber(
        &Event::ShellCommandProgress(tau_proto::ShellCommandProgress {
            command_id: tau_proto::ShellCommandId::parse("shell-replay").expect("command id"),
            stream: tau_proto::ShellStream::Stdout,
            chunk: "output".to_owned(),
            target_agent_id: Some(target),
        })
    ));
}
