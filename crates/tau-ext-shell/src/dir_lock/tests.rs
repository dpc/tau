use super::fs::{registry_generation, registry_waiter_count};
use super::*;

fn path(value: &str) -> PathBuf {
    PathBuf::from(value)
}

fn agent_id(value: &str) -> AgentId {
    AgentId::parse(value).expect("valid test agent id")
}

fn cbor_text_field<'a>(value: &'a CborValue, key: &str) -> Option<&'a str> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries
        .iter()
        .find_map(|(field, value)| match (field, value) {
            (CborValue::Text(field), CborValue::Text(value)) if field == key => {
                Some(value.as_str())
            }
            _ => None,
        })
}

fn cbor_int_field(value: &CborValue, key: &str) -> Option<i128> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries
        .iter()
        .find_map(|(field, value)| match (field, value) {
            (CborValue::Text(field), CborValue::Integer(value)) if field == key => {
                Some((*value).into())
            }
            _ => None,
        })
}

fn cbor_bool_field(value: &CborValue, key: &str) -> Option<bool> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries
        .iter()
        .find_map(|(field, value)| match (field, value) {
            (CborValue::Text(field), CborValue::Bool(value)) if field == key => Some(*value),
            _ => None,
        })
}

#[test]
fn dir_lock_result_omits_echoed_arguments() {
    let unchanged = dir_lock_result_value("/repo/a", Path::new("/repo/a"), Some(true));
    assert!(cbor_text_field(&unchanged, "command").is_none());
    assert!(cbor_text_field(&unchanged, "directory").is_none());
    assert!(cbor_text_field(&unchanged, "canonical_directory").is_none());
    assert_eq!(cbor_bool_field(&unchanged, "locked"), Some(true));

    let canonicalized =
        dir_lock_result_value("repo/../repo/a", Path::new("/tmp/repo/a"), Some(false));
    assert!(cbor_text_field(&canonicalized, "command").is_none());
    assert!(cbor_text_field(&canonicalized, "directory").is_none());
    assert_eq!(
        cbor_text_field(&canonicalized, "canonical_directory"),
        Some("/tmp/repo/a")
    );
    assert_eq!(cbor_bool_field(&canonicalized, "locked"), Some(false));
}

#[test]
fn path_conflicts_include_ancestors_and_children() {
    assert!(paths_overlap(Path::new("/tmp/a"), Path::new("/tmp/a")));
    assert!(paths_overlap(Path::new("/tmp/a"), Path::new("/tmp/a/b")));
    assert!(paths_overlap(Path::new("/tmp/a/b"), Path::new("/tmp/a")));
    assert!(!paths_overlap(Path::new("/tmp/a"), Path::new("/tmp/b")));
}

#[test]
fn shell_auto_lock_requires_current_manual_coverage() {
    let manager = DirLockManager::default();
    let owner = agent_id("agent-a");
    assert!(matches!(
        manager.acquire_auto_if_manual_covers(
            "auto-without-manual".into(),
            owner.clone(),
            vec![path("/repo")],
            || {},
        ),
        Err(LockAcquireError::NotCovered)
    ));

    manager
        .acquire_manual("manual".into(), owner.clone(), path("/repo"), || {})
        .expect("manual lock");
    let guard = manager
        .acquire_auto_if_manual_covers(
            "auto-with-manual".into(),
            owner,
            vec![path("/repo/src")],
            || {},
        )
        .expect("auto lock with manual coverage");
    drop(guard);
}

#[test]
fn fifo_front_waiter_blocks_later_independent_request() {
    let manager = DirLockManager::default();
    manager
        .acquire_manual(
            "manual-a".into(),
            agent_id("agent-a"),
            path("/repo/a"),
            || {},
        )
        .expect("manual lock");

    let first = std::thread::spawn({
        let manager = manager.clone();
        move || {
            manager.acquire_manual(
                "manual-root".into(),
                agent_id("agent-b"),
                path("/repo"),
                || {},
            )
        }
    });
    wait_until(|| manager.inner.state.lock().expect("state").waiters.len() == 1);

    let second = std::thread::spawn({
        let manager = manager.clone();
        move || {
            manager.acquire_auto(
                "auto-b".into(),
                agent_id("agent-c"),
                vec![path("/other")],
                || {},
            )
        }
    });
    wait_until(|| manager.inner.state.lock().expect("state").waiters.len() == 2);
    assert_eq!(
        manager.inner.state.lock().expect("state").automatic.len(),
        0,
        "later independent auto lock must not jump a blocked front waiter"
    );

    manager
        .unlock_manual(&agent_id("agent-a"), Path::new("/repo/a"))
        .expect("unlock");
    first.join().expect("first").expect("first acquired");
    manager
        .unlock_manual(&agent_id("agent-b"), Path::new("/repo"))
        .expect("unlock root");
    let guard = second.join().expect("second").expect("second acquired");
    drop(guard);
}

#[test]
fn manual_lock_rejects_same_owner_overlapping_lock_but_allows_auto_reentry() {
    let manager = DirLockManager::default();
    manager
        .acquire_manual(
            "manual-a".into(),
            agent_id("agent-a"),
            path("/repo/a"),
            || {},
        )
        .expect("manual lock");

    // A second manual lock by the same agent is usually a forgotten unlock,
    // so reject both exact and ancestor/child overlaps instead of hiding the
    // mistake behind extra lock ownership.
    assert_eq!(
        manager.acquire_manual(
            "manual-a-again".into(),
            agent_id("agent-a"),
            path("/repo/a"),
            || {}
        ),
        Err(ManualLockAcquireError::AlreadyHeld {
            dir: path("/repo/a")
        })
    );
    assert_eq!(
        manager.acquire_manual(
            "manual-a-child".into(),
            agent_id("agent-a"),
            path("/repo/a/child"),
            || {}
        ),
        Err(ManualLockAcquireError::AlreadyHeld {
            dir: path("/repo/a")
        })
    );
    assert_eq!(
        manager.acquire_manual(
            "manual-root".into(),
            agent_id("agent-a"),
            path("/repo"),
            || {}
        ),
        Err(ManualLockAcquireError::AlreadyHeld {
            dir: path("/repo/a")
        })
    );

    let first_guard = manager
        .acquire_auto(
            "auto-a".into(),
            agent_id("agent-a"),
            vec![path("/repo/a/child")],
            || {},
        )
        .expect("same-owner automatic tool reentry");

    // Same-owner automatic tools under a held manual lock are part of the
    // same writer critical section. They must not wait on an earlier
    // automatic call from that same agent, or a long-running shell would
    // deadlock follow-up writes by the lock owner.
    let second_guard = manager
        .acquire_auto(
            "auto-a-second".into(),
            agent_id("agent-a"),
            vec![path("/repo/a/child")],
            || panic!("same-owner automatic reentry should not wait"),
        )
        .expect("same-owner automatic tool reentry with active automatic lock");
    drop(second_guard);
    drop(first_guard);
}

#[test]
fn same_owner_auto_must_be_covered_by_manual_lock_to_reenter() {
    let manager = DirLockManager::default();
    manager
        .acquire_manual(
            "manual-child".into(),
            agent_id("agent-a"),
            path("/repo/a/child"),
            || {},
        )
        .expect("manual lock");

    let err = manager
        .acquire_auto(
            "auto-ancestor".into(),
            agent_id("agent-a"),
            vec![path("/repo/a")],
            || panic!("self-conflict should fail before waiting"),
        )
        .expect_err("uncovered same-owner auto should fail fast");

    assert_eq!(
        err,
        LockAcquireError::SelfConflict {
            dir: path("/repo/a/child")
        }
    );
    assert!(
        manager
            .inner
            .state
            .lock()
            .expect("state")
            .waiters
            .is_empty()
    );
    assert!(
        manager
            .inner
            .state
            .lock()
            .expect("state")
            .automatic
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn canonical_write_lock_dir_follows_chained_final_symlink() {
    // Automatic writer locks must target the same final file directory that
    // atomic writes will update through chained symlinks.
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let a = tempdir.path().join("a");
    let b = tempdir.path().join("b");
    let c = tempdir.path().join("c");
    std::fs::create_dir_all(&a).expect("mkdir a");
    std::fs::create_dir_all(&b).expect("mkdir b");
    std::fs::create_dir_all(&c).expect("mkdir c");
    std::fs::write(c.join("target.txt"), "old\n").expect("write target");
    std::os::unix::fs::symlink("../b/link2", a.join("link1")).expect("link1");
    std::os::unix::fs::symlink("../c/target.txt", b.join("link2")).expect("link2");

    let lock_dir = canonical_write_lock_dir(&a.join("link1")).expect("lock dir");

    assert_eq!(lock_dir, c.canonicalize().expect("canonical c"));
}

#[test]
fn disable_releases_manual_locks_and_cancels_waiters() {
    // Disabling dir_lock through config must not strand queued tools behind
    // locks that can no longer be unlocked through the disabled tool.
    let manager = DirLockManager::default();
    manager
        .acquire_manual(
            "manual-a".into(),
            agent_id("agent-a"),
            path("/repo/a"),
            || {},
        )
        .expect("manual lock");

    let waiter = std::thread::spawn({
        let manager = manager.clone();
        move || {
            manager.acquire_auto(
                "auto-b".into(),
                agent_id("agent-b"),
                vec![path("/repo/a")],
                || {},
            )
        }
    });
    wait_until(|| manager.inner.state.lock().expect("state").waiters.len() == 1);

    assert_eq!(manager.disable(), (1, 1));
    assert!(matches!(
        waiter.join().expect("waiter"),
        Err(LockAcquireError::Cancelled)
    ));
    assert!(manager.inner.state.lock().expect("state").manual.is_empty());
    assert!(
        manager
            .inner
            .state
            .lock()
            .expect("state")
            .waiters
            .is_empty()
    );

    manager
        .acquire_manual(
            "manual-after-disable".into(),
            agent_id("agent-c"),
            path("/repo/a"),
            || {},
        )
        .expect("no stale lock remains");
}

#[test]
fn release_agent_cancels_queued_waiters() {
    let manager = DirLockManager::default();
    manager
        .acquire_manual(
            "manual-a".into(),
            agent_id("agent-a"),
            path("/repo/a"),
            || {},
        )
        .expect("manual lock");

    let waiter = std::thread::spawn({
        let manager = manager.clone();
        move || {
            manager.acquire_manual(
                "manual-b".into(),
                agent_id("agent-b"),
                path("/repo/a"),
                || {},
            )
        }
    });
    wait_until(|| manager.inner.state.lock().expect("state").waiters.len() == 1);

    assert_eq!(manager.release_agent(&agent_id("agent-b")), 0);
    assert_eq!(
        waiter.join().expect("waiter"),
        Err(ManualLockAcquireError::Cancelled)
    );
    assert!(
        manager
            .inner
            .state
            .lock()
            .expect("state")
            .waiters
            .is_empty()
    );

    manager
        .unlock_manual(&agent_id("agent-a"), Path::new("/repo/a"))
        .expect("unlock");
    manager
        .acquire_manual(
            "manual-c".into(),
            agent_id("agent-c"),
            path("/repo/a"),
            || {},
        )
        .expect("cancelled waiter should not leave stale queue state");
}

#[test]
fn same_owner_automatic_locks_still_serialize_without_manual_lock() {
    let manager = DirLockManager::default();
    let guard = manager
        .acquire_auto(
            "auto-a".into(),
            agent_id("agent-a"),
            vec![path("/repo/a")],
            || {},
        )
        .expect("first automatic lock");

    // Reentry is tied to an explicit manual lock. Without one, overlapping
    // automatic tools still serialize even when they come from the same
    // agent.
    let second = std::thread::spawn({
        let manager = manager.clone();
        move || {
            manager.acquire_auto(
                "auto-a-second".into(),
                agent_id("agent-a"),
                vec![path("/repo/a/child")],
                || {},
            )
        }
    });
    wait_until(|| manager.inner.state.lock().expect("state").waiters.len() == 1);
    drop(guard);
    let second_guard = second.join().expect("second").expect("second acquired");
    drop(second_guard);
}

#[test]
fn abandoned_manual_lock_errors_after_liveness_check() {
    let manager = DirLockManager::default();
    manager
        .acquire_manual(
            "manual-a".into(),
            agent_id("agent-a"),
            path("/repo/a"),
            || {},
        )
        .expect("manual lock");
    make_manual_lock_stale(&manager, "/repo/a");

    // A waiter should eventually stop waiting on an idle manual lock and
    // report the exact abandoned owner and directory instead of hanging
    // forever behind a forgotten unlock.
    let err = manager
        .acquire_auto_with_policy(
            "auto-b".into(),
            agent_id("agent-b"),
            vec![path("/repo/a/child")],
            || {},
            fast_liveness_policy(),
        )
        .expect_err("stale manual lock should error");
    let LockAcquireError::Abandoned(lock) = err else {
        panic!("expected abandoned lock error");
    };
    assert_eq!(lock.owner.as_str(), "agent-a");
    assert_eq!(lock.dir, path("/repo/a"));
    assert!(Duration::from_secs(1) < lock.idle_for);

    let failure = lock.tool_failure();
    assert_eq!(failure.message, ABANDONED_LOCK_ERROR);
    assert!(!failure.message.contains("agent-a"));
    assert!(!failure.message.contains("/repo/a"));
    let details = failure.details.as_deref().expect("structured details");
    assert_eq!(
        cbor_text_field(details, "output"),
        Some(ABANDONED_LOCK_OUTPUT)
    );
    assert_eq!(
        cbor_text_field(details, "blocking_directory"),
        Some("/repo/a")
    );
    assert_eq!(cbor_text_field(details, "lock_owner_id"), Some("agent-a"));
    assert!(1 < cbor_int_field(details, "idle_seconds").expect("idle seconds"));
    assert!(1 < cbor_int_field(details, "held_seconds").expect("held seconds"));

    assert!(
        manager
            .inner
            .state
            .lock()
            .expect("state")
            .waiters
            .is_empty(),
        "abandoned waiter should be removed from the FIFO queue"
    );
}

#[test]
fn active_same_owner_auto_prevents_abandoned_lock_error() {
    let manager = DirLockManager::default();
    manager
        .acquire_manual(
            "manual-a".into(),
            agent_id("agent-a"),
            path("/repo/a"),
            || {},
        )
        .expect("manual lock");
    make_manual_lock_stale(&manager, "/repo/a");
    let guard = manager
        .acquire_auto(
            "auto-a".into(),
            agent_id("agent-a"),
            vec![path("/repo/a/child")],
            || {},
        )
        .expect("same-owner active automatic lock");

    // The manual lock is old, but it is not abandoned while the owner has a
    // mutating tool running inside it.
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn({
        let manager = manager.clone();
        move || {
            let result = manager.acquire_auto_with_policy(
                "auto-b".into(),
                agent_id("agent-b"),
                vec![path("/repo/a/child")],
                || {},
                fast_liveness_policy(),
            );
            let _ = tx.send(());
            result
        }
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(30)).is_err(),
        "waiter should stay blocked while owner has an active automatic tool"
    );

    drop(guard);
    manager
        .unlock_manual(&agent_id("agent-a"), Path::new("/repo/a"))
        .expect("unlock");
    let acquired = waiter.join().expect("waiter").expect("lock acquired");
    drop(acquired);
}

fn fast_liveness_policy() -> LockWaitPolicy {
    LockWaitPolicy {
        liveness_interval: Duration::from_millis(5),
        abandoned_after: Duration::from_millis(5),
    }
}

fn make_manual_lock_stale(manager: &DirLockManager, dir: &str) {
    let mut state = manager.inner.state.lock().expect("state");
    let lock = state
        .manual
        .iter_mut()
        .find(|lock| lock.dir == path(dir))
        .expect("manual lock");
    let old = Instant::now() - Duration::from_secs(5);
    lock.acquired_at = old;
    lock.last_used_at = old;
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !predicate() {
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn filesystem_lock_config(state_dir: &Path) -> DirLockConfig {
    DirLockConfig {
        enable: true,
        backend: DirLockBackendConfig::Filesystem,
        state_dir: Some(state_dir.to_path_buf()),
        enforce_ro_bind: true,
    }
}

fn filesystem_lock_manager(state_dir: &Path) -> DirLockManager {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))
            .expect("private state dir permissions");
    }
    let manager = DirLockManager::default();
    manager
        .configure(&filesystem_lock_config(state_dir))
        .expect("filesystem dir_lock backend");
    manager
}

/// Ensures the filesystem backend coordinates path-ancestor conflicts between
/// distinct `DirLockManager` instances, preventing separate ext-shell processes
/// from mutating the same subtree concurrently.
#[test]
fn filesystem_backend_blocks_conflicting_cross_instance_lock_until_unlock() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager_a = filesystem_lock_manager(tempdir.path());
    let manager_b = filesystem_lock_manager(tempdir.path());

    manager_a
        .acquire_manual(
            "manual-a".into(),
            agent_id("agent-a"),
            path("/repo/a"),
            || {},
        )
        .expect("manual lock a");

    let waiter = std::thread::spawn({
        let manager_b = manager_b.clone();
        move || {
            manager_b.acquire_manual("manual-b".into(), agent_id("agent-b"), path("/repo"), || {})
        }
    });
    std::thread::sleep(Duration::from_millis(150));
    assert!(!waiter.is_finished(), "cross-instance waiter must block");

    manager_a
        .unlock_manual(&agent_id("agent-a"), Path::new("/repo/a"))
        .expect("unlock a");
    waiter.join().expect("waiter").expect("waiter acquired");
}

/// Ensures non-overlapping sibling paths remain independent even when lock
/// state is persisted through the shared filesystem registry.
#[test]
fn filesystem_backend_allows_sibling_locks_cross_instance() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager_a = filesystem_lock_manager(tempdir.path());
    let manager_b = filesystem_lock_manager(tempdir.path());

    manager_a
        .acquire_manual(
            "manual-a".into(),
            agent_id("agent-a"),
            path("/repo/a"),
            || {},
        )
        .expect("manual lock a");
    manager_b
        .acquire_manual(
            "manual-b".into(),
            agent_id("agent-b"),
            path("/repo/b"),
            || panic!("sibling path should not wait"),
        )
        .expect("manual lock b");
}

/// Ensures cancellation removes this instance's persisted waiter so future
/// instances do not remain FIFO-blocked behind a dead queue entry.
#[test]
fn filesystem_backend_cancellation_removes_registry_waiter() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager_a = filesystem_lock_manager(tempdir.path());
    let manager_b = filesystem_lock_manager(tempdir.path());

    manager_a
        .acquire_manual("manual-a".into(), agent_id("agent-a"), path("/repo"), || {})
        .expect("manual lock a");
    let waiter = std::thread::spawn({
        let manager_b = manager_b.clone();
        move || {
            manager_b.acquire_manual("manual-b".into(), agent_id("agent-b"), path("/repo"), || {})
        }
    });
    wait_until(|| manager_b.cancel_waiting_call(&"manual-b".into()));
    assert!(matches!(
        waiter.join().expect("waiter"),
        Err(ManualLockAcquireError::Cancelled)
    ));
    manager_a
        .unlock_manual(&agent_id("agent-a"), Path::new("/repo"))
        .expect("unlock a");
    manager_b
        .acquire_manual(
            "manual-c".into(),
            agent_id("agent-c"),
            path("/repo"),
            || panic!("cancelled waiter should be gone"),
        )
        .expect("manual lock c");
}

/// Ensures a new filesystem backend instance reaps registry records whose
/// owning instance lease lock has been released by process/manager shutdown.
#[test]
fn filesystem_backend_reaps_dead_instance_locks() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    {
        let manager_a = filesystem_lock_manager(tempdir.path());
        manager_a
            .acquire_manual("manual-a".into(), agent_id("agent-a"), path("/repo"), || {})
            .expect("manual lock a");
        // Drop without shutdown to simulate an ext-shell process disappearing;
        // the instance lease is released and the registry entry should be
        // reaped.
    }
    let manager_b = filesystem_lock_manager(tempdir.path());
    manager_b
        .acquire_manual(
            "manual-b".into(),
            agent_id("agent-b"),
            path("/repo"),
            || panic!("dead instance should be reaped before waiting"),
        )
        .expect("manual lock b");
}

/// Ensures force-unlock removes overlapping manual locks from the shared
/// registry so blocked work in another ext-shell instance can proceed.
#[test]
fn filesystem_backend_force_unlock_propagates_across_instances() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager_a = filesystem_lock_manager(tempdir.path());
    let manager_b = filesystem_lock_manager(tempdir.path());
    manager_a
        .acquire_manual(
            "manual-a".into(),
            agent_id("agent-a"),
            path("/repo/a"),
            || {},
        )
        .expect("manual lock a");

    let removed = manager_b.force_unlock_overlapping(Path::new("/repo"));
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].owner, agent_id("agent-a"));
    assert_eq!(removed[0].dir, path("/repo/a"));
    manager_b
        .acquire_manual(
            "manual-b".into(),
            agent_id("agent-b"),
            path("/repo"),
            || panic!("force-unlocked registry should not block"),
        )
        .expect("manual lock b");
}

/// Ensures explicitly configured filesystem state directories fail closed when
/// they are not private, which prevents silent downgrade to process-local locks
/// or use of a shared world-readable registry.
#[cfg(unix)]
#[test]
fn filesystem_backend_rejects_insecure_configured_state_dir() {
    use std::os::unix::fs::PermissionsExt;

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::fs::set_permissions(tempdir.path(), std::fs::Permissions::from_mode(0o755))
        .expect("chmod tempdir");
    let manager = DirLockManager::default();
    let error = manager
        .configure(&filesystem_lock_config(tempdir.path()))
        .expect_err("insecure state dir should be rejected");
    assert!(error.contains("must be private"));
}

fn filesystem_registry_generation(state_dir: &Path) -> u64 {
    registry_generation(state_dir).expect("read registry")
}

/// Ensures the `owner_agent_id` recovery path uses the user-visible agent id to
/// release an exact manual lock held by another filesystem-backend instance.
#[test]
fn filesystem_backend_unlock_owner_agent_id_releases_cross_instance_lock() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager_a = filesystem_lock_manager(tempdir.path());
    let manager_b = filesystem_lock_manager(tempdir.path());
    manager_a
        .acquire_manual("manual-a".into(), agent_id("agent-a"), path("/repo"), || {})
        .expect("manual lock a");

    manager_b
        .unlock_manual_with_scope(
            &agent_id("agent-a"),
            Path::new("/repo"),
            UnlockOwnerScope::AnyInstanceWithAgentId,
        )
        .expect("cross-instance owner_agent_id unlock");
    manager_b
        .acquire_manual(
            "manual-b".into(),
            agent_id("agent-b"),
            path("/repo"),
            || panic!("owner_agent_id unlock should remove blocker"),
        )
        .expect("manual lock b");
}

/// Ensures equal agent-id text in different ext-shell instances does not grant
/// same-owner automatic reentry or duplicate-manual treatment across instances.
#[test]
fn filesystem_backend_same_text_agent_ids_are_distinct_owners() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager_a = filesystem_lock_manager(tempdir.path());
    let manager_b = filesystem_lock_manager(tempdir.path());
    manager_a
        .acquire_manual("manual-a".into(), agent_id("agent-a"), path("/repo"), || {})
        .expect("manual lock a");

    let waiter = std::thread::spawn({
        let manager_b = manager_b.clone();
        move || {
            manager_b.acquire_auto(
                "auto-b".into(),
                agent_id("agent-a"),
                vec![path("/repo")],
                || {},
            )
        }
    });
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !waiter.is_finished(),
        "same visible agent id in another instance must not reenter"
    );
    manager_a
        .unlock_manual(&agent_id("agent-a"), Path::new("/repo"))
        .expect("unlock a");
    let guard = waiter.join().expect("waiter").expect("auto acquired");
    drop(guard);
}

/// Ensures a failed filesystem backend reconfiguration reports an error without
/// clearing the active memory backend's lock state.
#[cfg(unix)]
#[test]
fn failed_filesystem_reconfigure_preserves_existing_memory_locks() {
    use std::os::unix::fs::PermissionsExt;

    let manager = DirLockManager::default();
    manager
        .acquire_manual("manual-a".into(), agent_id("agent-a"), path("/repo"), || {})
        .expect("memory manual lock");
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::fs::set_permissions(tempdir.path(), std::fs::Permissions::from_mode(0o755))
        .expect("chmod tempdir");

    assert!(
        manager
            .configure(&filesystem_lock_config(tempdir.path()))
            .is_err()
    );
    let waiter = std::thread::spawn({
        let manager = manager.clone();
        move || manager.acquire_manual("manual-b".into(), agent_id("agent-b"), path("/repo"), || {})
    });
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !waiter.is_finished(),
        "old memory lock must survive failed reconfigure"
    );
    manager
        .unlock_manual(&agent_id("agent-a"), Path::new("/repo"))
        .expect("unlock old memory lock");
    waiter.join().expect("waiter").expect("manual b acquired");
}

/// Ensures automatic guards retain the original filesystem lease and release
/// handle after backend disable/reconfiguration, so other instances remain
/// blocked until the running mutating tool drops its guard.
#[test]
fn filesystem_auto_guard_survives_backend_disable_until_drop() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager_a = filesystem_lock_manager(tempdir.path());
    let manager_b = filesystem_lock_manager(tempdir.path());
    let guard = manager_a
        .acquire_auto(
            "auto-a".into(),
            agent_id("agent-a"),
            vec![path("/repo")],
            || {},
        )
        .expect("auto lock a");
    manager_a
        .configure(&DirLockConfig::default())
        .expect("disable filesystem backend");

    let waiter = std::thread::spawn({
        let manager_b = manager_b.clone();
        move || {
            manager_b.acquire_manual("manual-b".into(), agent_id("agent-b"), path("/repo"), || {})
        }
    });
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !waiter.is_finished(),
        "original automatic lease should block until guard drops"
    );
    drop(guard);
    waiter.join().expect("waiter").expect("manual b acquired");
}

/// Ensures waiter polling is read-only after the initial enqueue and does not
/// bump the registry generation on every timed poll.
#[test]
fn filesystem_wait_polling_does_not_bump_registry_generation() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager_a = filesystem_lock_manager(tempdir.path());
    let manager_b = filesystem_lock_manager(tempdir.path());
    manager_a
        .acquire_manual("manual-a".into(), agent_id("agent-a"), path("/repo"), || {})
        .expect("manual lock a");
    let waiter = std::thread::spawn({
        let manager_b = manager_b.clone();
        move || {
            manager_b.acquire_manual("manual-b".into(), agent_id("agent-b"), path("/repo"), || {})
        }
    });
    wait_until(|| registry_waiter_count(tempdir.path()).expect("registry") == 1);
    let generation_after_enqueue = filesystem_registry_generation(tempdir.path());
    std::thread::sleep(Duration::from_millis(175));
    assert_eq!(
        filesystem_registry_generation(tempdir.path()),
        generation_after_enqueue,
        "polling without changes must not rewrite/bump registry"
    );
    manager_b.cancel_waiting_call(&"manual-b".into());
    assert!(matches!(
        waiter.join().expect("waiter"),
        Err(ManualLockAcquireError::Cancelled)
    ));
}
