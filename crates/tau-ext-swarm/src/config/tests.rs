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

/// Every configurable state and queue bound accepts its inclusive extrema and
/// reports the exact field when rejecting adjacent values.
#[test]
fn validates_all_limit_extrema() {
    /// One configurable state or queue bound and its test mutation.
    struct Limit {
        /// Fully qualified configuration field name.
        name: &'static str,
        /// Inclusive lower bound.
        minimum: usize,
        /// Inclusive upper bound.
        maximum: usize,
        /// Installs a candidate value in one Limits field.
        set: fn(&mut Limits, usize),
    }

    let Limits {
        command_entries: _,
        command_bytes: _,
        blocker_entries: _,
        blocker_bytes: _,
        update_entries: _,
        update_bytes: _,
        task_info_entries: _,
        change_history_entries: _,
        change_history_bytes: _,
        publication_bytes: _,
        agent_entries: _,
        watch_entries: _,
        submission_queue_entries: _,
    } = Limits::default();
    let limits = [
        Limit {
            name: "limits.command_entries",
            minimum: 1,
            maximum: 16_384,
            set: |limits, value| limits.command_entries = value,
        },
        Limit {
            name: "limits.command_bytes",
            minimum: 1,
            maximum: 256 * 1024 * 1024,
            set: |limits, value| limits.command_bytes = value,
        },
        Limit {
            name: "limits.blocker_entries",
            minimum: 1,
            maximum: 4_096,
            set: |limits, value| limits.blocker_entries = value,
        },
        Limit {
            name: "limits.blocker_bytes",
            minimum: 256 * 1024,
            maximum: 4 * 1024 * 1024,
            set: |limits, value| limits.blocker_bytes = value,
        },
        Limit {
            name: "limits.update_entries",
            minimum: 1,
            maximum: 4_096,
            set: |limits, value| limits.update_entries = value,
        },
        Limit {
            name: "limits.update_bytes",
            minimum: 256 * 1024,
            maximum: 64 * 1024 * 1024,
            set: |limits, value| limits.update_bytes = value,
        },
        Limit {
            name: "limits.task_info_entries",
            minimum: 1,
            maximum: tau_swarm_api::MAX_TASK_INFO_ENTRIES,
            set: |limits, value| limits.task_info_entries = value,
        },
        Limit {
            name: "limits.change_history_entries",
            minimum: 1,
            maximum: 65_536,
            set: |limits, value| limits.change_history_entries = value,
        },
        Limit {
            name: "limits.change_history_bytes",
            minimum: 1024 * 1024,
            maximum: 128 * 1024 * 1024,
            set: |limits, value| limits.change_history_bytes = value,
        },
        Limit {
            name: "limits.publication_bytes",
            minimum: 1024 * 1024,
            maximum: 8 * 1024 * 1024,
            set: |limits, value| limits.publication_bytes = value,
        },
        Limit {
            name: "limits.agent_entries",
            minimum: 1,
            maximum: 65_536,
            set: |limits, value| limits.agent_entries = value,
        },
        Limit {
            name: "limits.watch_entries",
            minimum: 1,
            maximum: 262_144,
            set: |limits, value| limits.watch_entries = value,
        },
        Limit {
            name: "limits.submission_queue_entries",
            minimum: 1,
            maximum: 64,
            set: |limits, value| limits.submission_queue_entries = value,
        },
    ];
    for limit in limits {
        for value in [limit.minimum, limit.maximum] {
            let mut configured = Limits::default();
            (limit.set)(&mut configured, value);
            configured.validate().expect("inclusive bound");
        }
        for value in [limit.minimum - 1, limit.maximum + 1] {
            let mut configured = Limits::default();
            (limit.set)(&mut configured, value);
            assert_eq!(
                configured.validate().expect_err("adjacent bound"),
                format!(
                    "{} is outside {}..={}",
                    limit.name, limit.minimum, limit.maximum
                )
            );
        }
    }
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

/// Raw JSON limit fields retain their exact accepted values after validation,
/// but the runtime receives unit-named internal limit groups instead.
#[test]
fn resolves_raw_limit_fields_into_named_internal_groups() {
    let config: ExtConfig = serde_json::from_value(serde_json::json!({
        "endpoint": {"peer_id": peer_id()},
        "credential_id": "worker",
        "credential_secret": "swarm",
        "hostname": "builder",
        "limits": {
            "command_entries": 7,
            "command_bytes": 262_145,
            "blocker_entries": 9,
            "blocker_bytes": 262_146,
            "update_entries": 11,
            "update_bytes": 262_147,
            "task_info_entries": 13,
            "change_history_entries": 15,
            "change_history_bytes": 1_048_576,
            "publication_bytes": 1_048_577,
            "agent_entries": 17,
            "watch_entries": 19,
            "submission_queue_entries": 21
        }
    }))
    .expect("stable JSON shape");
    let resolved = config
        .resolve(&BTreeMap::from([(
            "swarm".into(),
            SecretValue::new("secret"),
        )]))
        .expect("accepted configured values");

    assert_eq!(resolved.command_limits.entries, 7);
    assert_eq!(resolved.command_limits.logical_bytes, 262_145);
    assert_eq!(resolved.blocker_history_limits.entries, 9);
    assert_eq!(resolved.blocker_history_limits.encoded_bytes, 262_146);
    assert_eq!(resolved.update_limits.entries, 11);
    assert_eq!(resolved.update_limits.logical_bytes, 262_147);
    assert_eq!(
        resolved.projection_limits,
        crate::projection::ProjectionLimits {
            history_entries: 15,
            history_bytes: 1_048_576,
            publication_bytes: 1_048_577,
            task_info_entries: 13,
        }
    );
    assert_eq!(resolved.runtime_limits.agent_entries, 17);
    assert_eq!(resolved.runtime_limits.watch_entries, 19);
    assert_eq!(resolved.runtime_limits.submission_queue_entries, 21);
}
