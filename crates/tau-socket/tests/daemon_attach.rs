use std::time::Duration;

use tau_proto::{
    ClientKind, EventSelector, HarnessInputMessage, HarnessOutputMessage, Hello, PROTOCOL_VERSION,
    Subscribe,
};
use tau_socket::{SocketPeer, SocketReceive};
use tau_test_support::TestRuntime;

/// Ensures later-attached socket clients can drive daemon sessions and persist
/// the resulting conversation state.
#[test]
fn socket_transport_supports_later_attached_end_to_end_clients() {
    let runtime = TestRuntime::new().expect("runtime should be created");
    let daemon = runtime.spawn_daemon("session-1", Some(2));
    runtime
        .wait_until_ready(Duration::from_secs(2))
        .expect("daemon socket should appear");

    let first = runtime
        .send_daemon_message("session-1", "hello")
        .expect("first client message should succeed");
    let second = runtime
        .send_daemon_message("session-1", "read Cargo.toml")
        .expect("second client message should succeed");

    assert!(!first.is_empty(), "response should not be empty");
    assert!(!second.is_empty(), "read response should not be empty");
    daemon.join().expect("daemon should exit cleanly");

    let store = runtime
        .open_session_store()
        .expect("session store should reopen");
    let session = store.session("session-1").expect("session should exist");
    let agent_store = runtime
        .open_agent_store()
        .expect("agent store should reopen");
    // Optional AGENTS.md preambles + 2 × (user, tool.req, tool.res, agent).
    let entry_count: usize = session
        .loaded_agents()
        .into_iter()
        .filter_map(|agent_id| agent_store.agent(agent_id.as_str()))
        .map(|agent| agent.current_branch().len())
        .sum();
    assert!(
        (8..=12).contains(&entry_count),
        "expected two persisted prompt/tool cycles, got {entry_count} entries"
    );
    assert!(
        !runtime.state_dir.join("policy.cbor").exists(),
        "normal startup and socket attachment must not create legacy policy state"
    );
}

/// Ensures a forbidden socket subscription disconnects only the denied client
/// while the daemon continues serving valid clients.
#[test]
fn forbidden_socket_subscription_disconnects_client_without_killing_daemon() {
    let runtime = TestRuntime::new().expect("runtime should be created");
    let legacy_policy_path = runtime.state_dir.join("policy.cbor");
    let legacy_bytes = b"corrupt legacy policy bytes";
    std::fs::write(&legacy_policy_path, legacy_bytes).expect("write legacy policy marker");
    let daemon = runtime.spawn_daemon("session-1", Some(2));
    runtime
        .wait_until_ready(Duration::from_secs(2))
        .expect("daemon socket should appear");

    let mut denied_client =
        SocketPeer::connect(&runtime.socket_path).expect("denied client should connect");
    denied_client
        .send(&HarnessInputMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: tau_proto::ExtensionName::parse("denied-client")
                .expect("test identifier must satisfy its grammar"),
            client_kind: ClientKind::Ui,
            expected_session_id: None,
            capabilities: Default::default(),
        }))
        .expect("hello should send");
    denied_client
        .send(&HarnessInputMessage::Subscribe(Subscribe {
            // `unknown.` is not an allowed event family; sockets may only
            // subscribe to the closed list of well-known categories.
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Prefix("unknown.".to_owned())],
        }))
        .expect("forbidden subscribe should send");

    let denial = denied_client
        .recv_timeout(Duration::from_secs(2))
        .expect("daemon should reply to denied client");
    let SocketReceive::Message {
        message: HarnessOutputMessage::Disconnect(disconnect),
    } = denial
    else {
        panic!("expected disconnect message, got {denial:?}");
    };
    let reason = disconnect
        .reason
        .expect("disconnect reason should be present");
    assert!(reason.contains("subscription denied"));

    let response = runtime
        .send_daemon_message("session-1", "hello")
        .expect("daemon should still serve valid clients");
    assert!(!response.is_empty(), "response should not be empty");
    daemon.join().expect("daemon should exit cleanly");

    assert_eq!(
        std::fs::read(&legacy_policy_path).expect("read legacy policy marker"),
        legacy_bytes,
        "startup and subscription handling must ignore legacy policy state"
    );
}
