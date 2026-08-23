use std::collections::BTreeMap;

use super::*;

fn peer_id() -> String {
    iroh::SecretKey::generate().public().to_string()
}

/// Ensures identity-only endpoint configuration remains valid while the
/// credential secret still comes exclusively from Configure.
#[test]
fn resolves_identity_only_endpoint_and_secret() {
    let config: ExtConfig = serde_json::from_value(serde_json::json!({
        "endpoint": {"peer_id": peer_id()},
        "credential_id": "worker",
        "credential_secret": "swarm",
        "hostname": "builder-01"
    }))
    .expect("strict config");
    let secrets = BTreeMap::from([("swarm".into(), SecretValue::new("secret"))]);
    let resolved = config.resolve(&secrets).expect("valid config");
    assert!(resolved.endpoint.addrs.is_empty());
    assert_eq!(resolved.credential.secret.expose(), b"secret");
}

/// Ensures nested unknown fields fail instead of being silently ignored and
/// changing the intended endpoint contract.
#[test]
fn rejects_unknown_nested_config() {
    let result = serde_json::from_value::<ExtConfig>(serde_json::json!({
        "endpoint": {"peer_id": peer_id(), "expected_peer": peer_id()},
        "credential_id": "worker",
        "credential_secret": "swarm"
    }));
    assert!(result.is_err());
}

/// Ensures reconnect delays cannot invert the approved bounded backoff
/// interval.
#[test]
fn rejects_inverted_reconnect_range() {
    let config: ExtConfig = serde_json::from_value(serde_json::json!({
        "endpoint": {"peer_id": peer_id()},
        "credential_id": "worker",
        "credential_secret": "swarm",
        "hostname": "builder",
        "reconnect": {"initial_delay_ms": 1000, "maximum_delay_ms": 10, "jitter_per_mille": 0}
    }))
    .expect("shape");
    let secrets = BTreeMap::from([("swarm".into(), SecretValue::new("secret"))]);
    assert!(config.resolve(&secrets).is_err());
}

/// Credentials must come from the Configure secret map; ambient process state
/// is never a fallback.
#[test]
fn rejects_missing_configure_secret() {
    let config: ExtConfig = serde_json::from_value(serde_json::json!({
        "endpoint": {"peer_id": peer_id()},
        "credential_id": "worker",
        "credential_secret": "missing",
        "hostname": "builder"
    }))
    .expect("shape");
    assert!(config.resolve(&BTreeMap::new()).is_err());
}

/// Route hints are strict and duplicates do not silently collapse.
#[test]
fn rejects_duplicate_direct_address() {
    let config: ExtConfig = serde_json::from_value(serde_json::json!({
        "endpoint": {
            "peer_id": peer_id(),
            "direct_addresses": ["127.0.0.1:1234", "127.0.0.1:1234"]
        },
        "credential_id": "worker",
        "credential_secret": "swarm",
        "hostname": "builder"
    }))
    .expect("shape");
    let secrets = BTreeMap::from([("swarm".into(), SecretValue::new("secret"))]);
    assert!(config.resolve(&secrets).is_err());
}

/// Corrected command, blocker, and update byte ceilings accept their inclusive
/// extrema.
#[test]
fn accepts_corrected_state_byte_extrema() {
    let limits = Limits {
        command_bytes: 1,
        blocker_bytes: 256 * 1024,
        update_bytes: 64 * 1024 * 1024,
        ..Limits::default()
    };
    limits.validate().expect("inclusive corrected extrema");
    Limits {
        command_bytes: 0,
        ..Limits::default()
    }
    .validate()
    .expect_err("zero command bytes");
    Limits {
        command_bytes: 256 * 1024 * 1024,
        ..Limits::default()
    }
    .validate()
    .expect("maximum command bytes");
}

/// Local task-info retention may be lowered but can never exceed the shared
/// protocol ceiling, preventing Tau from constructing a peer-invalid snapshot.
#[test]
fn task_info_entry_limit_cannot_exceed_protocol_maximum() {
    Limits {
        task_info_entries: 1,
        ..Limits::default()
    }
    .validate()
    .expect("minimum task-info entries");
    Limits {
        task_info_entries: 4_096,
        ..Limits::default()
    }
    .validate()
    .expect("maximum task-info entries");
    for task_info_entries in [0, 4_097] {
        Limits {
            task_info_entries,
            ..Limits::default()
        }
        .validate()
        .expect_err("out-of-range task-info entries");
    }
}
