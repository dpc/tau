//! Process-level regressions for tracing journals whose writer remains loaded.

mod support;

use std::path::PathBuf;
use std::process::Command;

use tau_core::AgentStore;
use tau_proto::{AgentCreator, AgentId, AgentStarted, Event};

/// Returns the bundled Tau binary under Cargo's integration-test contract.
fn tau_bin() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_tau")
        .map(PathBuf::from)
        .expect("CARGO_BIN_EXE_tau")
}

/// Appends one durable creation fact while retaining the store's writer lease.
fn start_agent(
    store: &mut AgentStore,
    agent_id: &AgentId,
    creator: AgentCreator,
    parent_agent: Option<AgentId>,
) {
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::AgentStarted(AgentStarted {
                agent_id: agent_id.clone(),
                creator: Some(creator),
                parent_agent,
                role: "trace-cli-test".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("agent start");
}

/// Runs the real CLI trace command against an externally held writer lock.
fn trace(root: &std::path::Path, agents_dir: &std::path::Path, agent_id: &AgentId) -> Vec<u8> {
    let output = support::isolated_tau_command(tau_bin(), root)
        .args([
            "agent",
            "trace",
            agent_id.as_str(),
            "--agents-dir",
            agents_dir.to_str().expect("UTF-8 fixture path"),
            "--format",
            "agent-performance-jsonl",
        ])
        .output()
        .expect("run tau agent trace");
    assert!(
        output.status.success(),
        "trace failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// A running target traces through its finite checkpoint and accepts later
/// work.
#[test]
fn cli_traces_running_agent_without_invalidating_writer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let agent_id = AgentId::parse("agent-running").expect("agent id");
    let mut store = AgentStore::open_lazy(&agents_dir).expect("store");
    start_agent(&mut store, &agent_id, AgentCreator::User, None);

    let output = trace(temp.path(), &agents_dir, &agent_id);
    let rows = String::from_utf8(output).expect("JSONL");
    assert_eq!(rows.lines().count(), 2, "finite header and summary");

    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                agent_id: agent_id.clone(),
                display_name: "later-work".to_owned(),
            }),
        )
        .expect("later routing/write survives");
}

/// A completed delegated worker remains traceable while both loaded leases and
/// descendant routing continue to work.
#[test]
fn cli_traces_completed_loaded_descendant_without_disrupting_parent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let parent = AgentId::parse("agent-parent").expect("parent id");
    let child = AgentId::parse("agent-completed-child").expect("child id");
    let session_id = tau_proto::SessionId::parse("trace-cli-session").expect("session id");
    let mut store = AgentStore::open_lazy(&agents_dir).expect("store");
    start_agent(&mut store, &parent, AgentCreator::User, None);
    start_agent(
        &mut store,
        &child,
        AgentCreator::Agent {
            agent_id: parent.clone(),
            session_id,
        },
        Some(parent.clone()),
    );
    store
        .append_agent_event(
            child.as_str(),
            None,
            Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                agent_id: child.clone(),
                display_name: "completed delegated worker".to_owned(),
            }),
        )
        .expect("completed worker's final durable fact");

    let output = Command::new(tau_bin())
        .args([
            "agent",
            "trace",
            parent.as_str(),
            "--include-descendants",
            "--agents-dir",
            agents_dir.to_str().expect("UTF-8 fixture path"),
            "--format",
            "tau-jsonl",
        ])
        .env_clear()
        .env("HOME", temp.path())
        .output()
        .expect("run descendant trace");
    assert!(
        output.status.success(),
        "trace failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = String::from_utf8(output.stdout).expect("JSONL");
    assert!(rows.contains(parent.as_str()));
    assert!(rows.contains(child.as_str()));

    store
        .append_agent_event(
            parent.as_str(),
            None,
            Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                agent_id: parent.clone(),
                display_name: "parent-still-routable".to_owned(),
            }),
        )
        .expect("parent later routing/write survives");
}
