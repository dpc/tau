use super::*;

/// Ensures the typed counter defaults retain the exact legacy registry schema,
/// zero values, collection defaults, and compact JSON field order.
#[test]
fn semantic_ids_preserve_exact_default_registry_json() {
    assert_eq!(
        serde_json::to_string(&FsRegistry::default()).expect("encode default registry"),
        concat!(
            r#"{"version":0,"generation":0,"next_waiter_id":0,"next_auto_id":0,"#,
            r#""manual":[],"automatic":[],"waiters":[]}"#
        )
    );
}

/// Ensures the private semantic identity wrappers preserve the exact legacy
/// registry schema, JSON number representation, and full `u64` value range.
#[test]
fn semantic_ids_preserve_exact_registry_json_and_full_u64_range() {
    const RAW_REGISTRY: &str = concat!(
        r#"{"version":0,"generation":18446744073709551615,"next_waiter_id":42,"#,
        r#""next_auto_id":18446744073709551615,"manual":[{"owner":{"instance_id":"instance","#,
        r#""agent_id":"agent"},"dir":"/tmp/locked","acquired_at_ms":1,"last_used_at_ms":2,"#,
        r#""active_auto_ids":[18446744073709551615]}],"automatic":[{"id":18446744073709551615,"#,
        r#""owner":{"instance_id":"instance","agent_id":"agent"},"dirs":["/tmp/locked"]}],"#,
        r#""waiters":[{"id":42,"call_id":"call","owner":{"instance_id":"instance","#,
        r#""agent_id":"agent"},"dirs":["/tmp/waiting"],"kind":"Manual"}]}"#
    );

    let registry: FsRegistry = serde_json::from_str(RAW_REGISTRY).expect("decode legacy registry");

    let _: FsRegistryGeneration = registry.generation;
    let _: DirLockWaiterId = registry.next_waiter_id;
    let _: DirLockAutoId = registry.next_auto_id;
    assert_eq!(registry.generation.into_raw(), u64::MAX);
    assert_eq!(registry.next_waiter_id.0, 42);
    assert_eq!(registry.next_auto_id.0, u64::MAX);
    assert_eq!(registry.manual[0].active_auto_ids[0].0, u64::MAX);
    assert_eq!(registry.automatic[0].id.0, u64::MAX);
    assert_eq!(registry.waiters[0].id.0, 42);
    assert_eq!(
        serde_json::to_string(&registry).expect("encode typed registry"),
        RAW_REGISTRY
    );
}

/// Ensures saturated filesystem counters retain the maximum valid raw value
/// instead of introducing a new invalid or wrapped identity.
#[test]
fn semantic_id_filesystem_counters_saturate_at_full_u64_range() {
    assert_eq!(
        DirLockWaiterId::from_raw(u64::MAX).saturating_next().0,
        u64::MAX
    );
    assert_eq!(
        DirLockAutoId::from_raw(u64::MAX).saturating_next().0,
        u64::MAX
    );
    assert_eq!(
        FsRegistryGeneration::from_raw(u64::MAX)
            .saturating_next()
            .into_raw(),
        u64::MAX
    );
}
