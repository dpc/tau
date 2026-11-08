use super::*;

/// Queue pressure and a dead worker never block canonical observation
/// admission.
#[test]
fn full_or_disconnected_writer_drops_without_waiting() {
    let (sender, receiver) = sync_channel(1);
    let writer = ProviderUsageWriter { sender };
    let snapshot: tau_proto::HarnessProviderQuotaChanged =
        serde_json::from_value(serde_json::json!({
            "provider": "chatgpt",
            "profile_epoch": "epoch",
            "sequence": 1,
            "windows": [{
                "key": {"limit_id": "codex", "window_id": "primary"},
                "used_basis_points": 2500,
                "usage_observed_at_unix_ms": 1000,
                "window_seconds": 18000
            }],
            "route_bindings": []
        }))
        .expect("valid quota fixture");
    writer.offer(&snapshot);
    writer.offer(&snapshot);
    assert!(receiver.try_recv().is_ok());
    assert!(receiver.try_recv().is_err());
    drop(receiver);
    writer.offer(&snapshot);
}
