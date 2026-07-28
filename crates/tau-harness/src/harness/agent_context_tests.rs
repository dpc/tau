use super::*;

fn publish(
    store: &mut AgentContextStore,
    agent: &str,
    key: &str,
    contributor: &str,
    extension_name: &str,
    value: serde_json::Value,
) {
    store.publish(
        tau_proto::AgentId::parse(agent).expect("agent id"),
        tau_proto::AgentContextKey::from(key),
        crate::test_connection_id(contributor),
        extension_name.to_owned(),
        tau_proto::AgentContextValue(value),
    );
}

fn template_value(store: &AgentContextStore, agent: &str) -> serde_json::Value {
    let agent_id = tau_proto::AgentId::parse(agent).expect("agent id");
    store.template_value(Some(&agent_id))
}

/// Contributions are isolated by `(agent, key, contributor)` so one
/// extension can publish multiple keys without overwriting another.
#[test]
fn publish_stores_per_agent_key_and_contributor() {
    let mut store = AgentContextStore::default();
    publish(
        &mut store,
        "agent-1",
        "skills",
        "c1",
        "alpha",
        serde_json::json!([1]),
    );
    publish(
        &mut store,
        "agent-1",
        "project",
        "c1",
        "alpha",
        serde_json::json!({"root": "/repo"}),
    );
    publish(
        &mut store,
        "agent-1",
        "skills",
        "c2",
        "beta",
        serde_json::json!([2]),
    );

    let visible = template_value(&store, "agent-1");

    assert_eq!(visible["skills"].as_array().expect("skills").len(), 2);
    assert_eq!(visible["project"].as_array().expect("project").len(), 1);
}

/// Republishing the same `(agent, key, contributor)` replaces the
/// contributor's previous JSON value instead of appending a duplicate.
#[test]
fn same_contributor_replaces_own_value() {
    let mut store = AgentContextStore::default();
    publish(
        &mut store,
        "agent-1",
        "skills",
        "c1",
        "alpha",
        serde_json::json!(["old"]),
    );
    publish(
        &mut store,
        "agent-1",
        "skills",
        "c1",
        "alpha",
        serde_json::json!(["new"]),
    );

    let visible = template_value(&store, "agent-1");

    assert_eq!(
        visible["skills"],
        serde_json::json!([{ "extension_name": "alpha", "value": ["new"] }])
    );
}

/// Multiple contributors for the same key are exposed as stable wrapper
/// objects sorted by extension name and then connection id.
#[test]
fn multiple_contributors_are_stable_wrappers_under_same_key() {
    let mut store = AgentContextStore::default();
    publish(
        &mut store,
        "agent-1",
        "skills",
        "c-z",
        "zeta",
        serde_json::json!([3]),
    );
    publish(
        &mut store,
        "agent-1",
        "skills",
        "c-a",
        "alpha",
        serde_json::json!([1]),
    );
    publish(
        &mut store,
        "agent-1",
        "skills",
        "c-b",
        "alpha",
        serde_json::json!([2]),
    );

    let visible = template_value(&store, "agent-1");

    assert_eq!(
        visible["skills"],
        serde_json::json!([
            { "extension_name": "alpha", "value": [1] },
            { "extension_name": "alpha", "value": [2] },
            { "extension_name": "zeta", "value": [3] },
        ])
    );
}

/// Agent context never leaks between agents, which matters when one session can
/// contain agents with different working directories.
#[test]
fn different_agents_do_not_leak_context() {
    let mut store = AgentContextStore::default();
    publish(
        &mut store,
        "agent-1",
        "skills",
        "c1",
        "alpha",
        serde_json::json!(["agent-1"]),
    );
    publish(
        &mut store,
        "agent-2",
        "skills",
        "c1",
        "alpha",
        serde_json::json!(["agent-2"]),
    );

    let agent_1 = template_value(&store, "agent-1");
    let agent_2 = template_value(&store, "agent-2");

    assert_eq!(
        agent_1["skills"][0]["value"],
        serde_json::json!(["agent-1"])
    );
    assert_eq!(
        agent_2["skills"][0]["value"],
        serde_json::json!(["agent-2"])
    );
}

/// Disconnect cleanup removes a contributor everywhere and prunes empty maps.
#[test]
fn remove_contributor_prunes_empty_agent_and_key_maps() {
    let mut store = AgentContextStore::default();
    publish(
        &mut store,
        "agent-1",
        "skills",
        "stale-ext",
        "stale",
        serde_json::json!(["old"]),
    );
    publish(
        &mut store,
        "agent-1",
        "skills",
        "live-ext",
        "live",
        serde_json::json!(["new"]),
    );
    publish(
        &mut store,
        "agent-2",
        "project",
        "stale-ext",
        "stale",
        serde_json::json!({"root": "/old"}),
    );

    store.remove_contributor(
        &tau_proto::ConnectionId::parse("stale-ext")
            .expect("test connection id must satisfy the identifier grammar"),
    );

    assert_eq!(
        template_value(&store, "agent-1")["skills"],
        serde_json::json!([{ "extension_name": "live", "value": ["new"] }])
    );
    assert_eq!(template_value(&store, "agent-2"), serde_json::json!({}));
}

#[test]
fn clear_removes_all_contributions() {
    let mut store = AgentContextStore::default();
    publish(
        &mut store,
        "agent-1",
        "skills",
        "ext",
        "ext",
        serde_json::json!(["value"]),
    );

    store.clear();

    assert_eq!(template_value(&store, "agent-1"), serde_json::json!({}));
}
