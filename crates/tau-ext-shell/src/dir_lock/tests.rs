use std::os::unix as path_std_os_unix;
use std::{
    fs as path_std_fs, process as path_std_process, sync as path_std_sync, time as path_std_time,
};

use super::fs::{
    liveness_cap_consumes_backoff_for_test, registry_generation, registry_waiter_count,
    set_fail_reap_for_test, wait_after_observed_wake_for_test, wait_backoff_delays_for_test,
};
use super::*;

fn path(value: &str) -> PathBuf {
    PathBuf::from(value)
}

fn agent_id(value: &str) -> AgentId {
    AgentId::parse(value).expect("valid test agent id")
}

/// Preserve exact command conversion and parser error precedence while making
/// every successfully parsed directory-lock request hold a valid command.
#[test]
fn dir_lock_request_retains_only_valid_commands_after_existing_directory_checks() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let canonical_dir = tempdir.path().canonicalize().expect("canonical tempdir");
    let file = tempdir.path().join("file");
    path_std_fs::write(&file, b"file").expect("write regular file");
    let missing = tempdir.path().join("missing");

    for (raw, expected) in [
        ("update", Some(DirLockCommand::Update)),
        ("unlock", Some(DirLockCommand::Unlock)),
        ("", None),
        ("Update", None),
        ("unlock ", None),
        ("other", None),
    ] {
        let parsed = DirLockCommand::try_from(raw).ok();
        assert_eq!(parsed, expected, "raw command {raw:?}");
        if let Some(command) = parsed {
            assert_eq!(command.as_str(), raw);
        }
    }

    let text = |value: &str| CborValue::Text(value.to_owned());
    let map = |entries: Vec<(&str, CborValue)>| {
        CborValue::Map(
            entries
                .into_iter()
                .map(|(key, value)| (text(key), value))
                .collect(),
        )
    };
    let invoke = |arguments| ToolStarted {
        invocation_policy: Default::default(),
        call_id: ToolCallId::new("dir-lock-request"),
        tool_name: tau_proto::ToolName::new(DIR_LOCK_TOOL_NAME),
        arguments,
        agent_id: agent_id("agent-a"),
        originator: tau_proto::PromptOriginator::User,
    };

    for (raw, expected) in [
        ("update", DirLockCommand::Update),
        ("unlock", DirLockCommand::Unlock),
    ] {
        let request = DirLockToolRequest::parse(&invoke(map(vec![
            ("command", text(raw)),
            (
                "directory",
                text(tempdir.path().to_str().expect("UTF-8 tempdir")),
            ),
        ])))
        .expect("valid directory-lock request");
        assert_eq!(request.command, expected);
        assert_eq!(request.command.as_str(), raw);
        assert_eq!(request.dir, canonical_dir);
    }

    let missing_error = missing
        .canonicalize()
        .expect_err("test path must remain absent");
    let cases = [
        (
            CborValue::Map(Vec::new()),
            "missing string argument: command".to_owned(),
            None,
        ),
        (
            map(vec![("command", CborValue::Integer(1.into()))]),
            "argument `command` must be a string".to_owned(),
            None,
        ),
        (
            map(vec![("command", text("invalid"))]),
            "missing string argument: directory".to_owned(),
            None,
        ),
        (
            map(vec![
                ("command", text("invalid")),
                ("directory", CborValue::Integer(1.into())),
            ]),
            "argument `directory` must be a string".to_owned(),
            None,
        ),
        (
            map(vec![
                ("command", text("invalid")),
                (
                    "directory",
                    text(missing.to_str().expect("UTF-8 missing path")),
                ),
            ]),
            format!(
                "directory {} does not exist: {missing_error}",
                missing.display()
            ),
            Some(missing.display().to_string()),
        ),
        (
            map(vec![
                ("command", text("invalid")),
                ("directory", text(file.to_str().expect("UTF-8 file path"))),
            ]),
            format!(
                "{} is not a directory",
                file.canonicalize().expect("canonical file").display()
            ),
            Some(file.display().to_string()),
        ),
        (
            map(vec![
                ("command", text("invalid")),
                (
                    "directory",
                    text(tempdir.path().to_str().expect("UTF-8 tempdir")),
                ),
            ]),
            "argument `command` must be `update` or `unlock`".to_owned(),
            Some(dir_lock_display_args("invalid", &canonical_dir)),
        ),
    ];

    for (arguments, expected_message, expected_args) in cases {
        let invoke = invoke(arguments.clone());
        let error = match DirLockToolRequest::parse(&invoke) {
            Ok(_) => panic!("request must fail"),
            Err(error) => error,
        };
        let Event::ToolError(error) = *error else {
            panic!("expected tool error");
        };
        assert_eq!(error.message, expected_message);
        assert_eq!(error.details, Some(arguments));
        let display = error.display.expect("error display");
        assert_eq!(display.args, expected_args.unwrap_or_default());
        assert_eq!(display.status, ToolUseStatus::Error);
        assert_eq!(display.status_text, "dir_lock failed");
    }
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

/// Ensures unrelated waiters are not blocked behind a queued request for a
/// different subtree while fairness is still preserved for overlapping paths.
#[test]
fn blocked_waiter_does_not_block_later_independent_request() {
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
                "manual-a-child".into(),
                agent_id("agent-b"),
                path("/repo/a/child"),
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
    let guard = second.join().expect("second").expect("second acquired");
    assert_eq!(
        manager.inner.state.lock().expect("state").waiters.len(),
        1,
        "later independent auto lock must not stay blocked behind an unrelated waiter"
    );
    drop(guard);

    manager
        .unlock_manual(&agent_id("agent-a"), Path::new("/repo/a"))
        .expect("unlock");
    first.join().expect("first").expect("first acquired");
    manager
        .unlock_manual(&agent_id("agent-b"), Path::new("/repo/a/child"))
        .expect("unlock child");
}

/// Ensures a later waiter that overlaps an earlier queued waiter cannot jump
/// ahead just because it does not overlap the currently active lock.
#[test]
fn overlapping_waiter_stays_behind_earlier_overlapping_waiter() {
    let manager = DirLockManager::default();
    manager
        .acquire_manual(
            "manual-a".into(),
            agent_id("agent-a"),
            path("/repo/a"),
            || {},
        )
        .expect("manual lock");

    let (first_tx, first_rx) = path_std_sync::mpsc::channel();
    let first = std::thread::spawn({
        let manager = manager.clone();
        move || {
            let result = manager.acquire_manual(
                "manual-root".into(),
                agent_id("agent-b"),
                path("/repo"),
                || {},
            );
            first_tx.send(result).expect("send first waiter result");
        }
    });
    wait_until(|| manager.inner.state.lock().expect("state").waiters.len() == 1);

    let (second_tx, second_rx) = path_std_sync::mpsc::channel();
    let second = std::thread::spawn({
        let manager = manager.clone();
        move || {
            let result = manager.acquire_manual(
                "manual-b".into(),
                agent_id("agent-c"),
                path("/repo/b"),
                || {},
            );
            second_tx.send(result).expect("send second waiter result");
        }
    });
    wait_until(|| manager.inner.state.lock().expect("state").waiters.len() == 2);
    assert!(
        second_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "later overlapping waiter must stay behind earlier overlapping waiter"
    );

    manager
        .unlock_manual(&agent_id("agent-a"), Path::new("/repo/a"))
        .expect("unlock blocker");
    first_rx
        .recv_timeout(Duration::from_millis(50))
        .expect("earlier overlapping waiter should acquire")
        .expect("earlier overlapping waiter result");
    assert!(
        second_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "later overlapping waiter must remain blocked until earlier waiter unlocks"
    );

    manager
        .unlock_manual(&agent_id("agent-b"), Path::new("/repo"))
        .expect("unlock root");
    second_rx
        .recv_timeout(Duration::from_millis(50))
        .expect("later overlapping waiter should acquire after root unlock")
        .expect("later overlapping waiter result");
    manager
        .unlock_manual(&agent_id("agent-c"), Path::new("/repo/b"))
        .expect("unlock sibling");
    first.join().expect("first waiter thread");
    second.join().expect("second waiter thread");
}

/// Ensures queued manual requests revalidate same-owner overlap before grant so
/// path-local queue scans cannot create duplicate manual locks for one agent.
#[test]
fn queued_same_owner_manual_waiter_errors_instead_of_duplicate_lock() {
    let manager = DirLockManager::default();
    manager
        .acquire_manual(
            "manual-root".into(),
            agent_id("agent-x"),
            path("/repo"),
            || {},
        )
        .expect("blocking manual lock");

    let (first_tx, first_rx) = path_std_sync::mpsc::channel();
    let first = std::thread::spawn({
        let manager = manager.clone();
        move || {
            let result = manager.acquire_manual(
                "manual-a".into(),
                agent_id("agent-a"),
                path("/repo/a"),
                || {},
            );
            first_tx.send(result).expect("send first waiter result");
        }
    });
    wait_until(|| manager.inner.state.lock().expect("state").waiters.len() == 1);

    let (second_tx, second_rx) = path_std_sync::mpsc::channel();
    let second = std::thread::spawn({
        let manager = manager.clone();
        move || {
            let result = manager.acquire_manual(
                "manual-a-child".into(),
                agent_id("agent-a"),
                path("/repo/a/child"),
                || {},
            );
            second_tx.send(result).expect("send second waiter result");
        }
    });
    wait_until(|| manager.inner.state.lock().expect("state").waiters.len() == 2);

    manager
        .unlock_manual(&agent_id("agent-x"), Path::new("/repo"))
        .expect("unlock blocker");
    first_rx
        .recv_timeout(Duration::from_millis(50))
        .expect("first same-owner waiter should acquire")
        .expect("first same-owner waiter result");
    assert_eq!(
        second_rx
            .recv_timeout(Duration::from_millis(50))
            .expect("second same-owner waiter should fail"),
        Err(ManualLockAcquireError::AlreadyHeld {
            dir: path("/repo/a")
        })
    );

    manager
        .unlock_manual(&agent_id("agent-a"), Path::new("/repo/a"))
        .expect("unlock acquired same-owner lock");
    first.join().expect("first waiter thread");
    second.join().expect("second waiter thread");
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
    path_std_os_unix::fs::symlink("../b/link2", a.join("link1")).expect("link1");
    path_std_os_unix::fs::symlink("../c/target.txt", b.join("link2")).expect("link2");

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
    let (tx, rx) = path_std_sync::mpsc::channel();
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
    let start = path_std_time::Instant::now();
    while !predicate() {
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        std::thread::sleep(path_std_time::Duration::from_millis(5));
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

fn disabled_filesystem_lock_config(state_dir: &Path) -> DirLockConfig {
    DirLockConfig {
        enable: false,
        backend: DirLockBackendConfig::Filesystem,
        state_dir: Some(state_dir.to_path_buf()),
        enforce_ro_bind: true,
    }
}

fn filesystem_lock_manager(state_dir: &Path) -> DirLockManager {
    std::fs::create_dir_all(state_dir).expect("create state dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(state_dir, path_std_fs::Permissions::from_mode(0o700))
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

/// Ensures filesystem `release_agent` notifies same-process waiters even when
/// it only cancels a queued waiter and does not release any manual lock.
#[test]
fn filesystem_backend_release_agent_notifies_queued_only_cancellation() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager_a = filesystem_lock_manager(tempdir.path());
    let manager_b = filesystem_lock_manager(tempdir.path());

    manager_a
        .acquire_manual("manual-a".into(), agent_id("agent-a"), path("/repo"), || {})
        .expect("manual lock a");
    let (tx, rx) = path_std_sync::mpsc::channel();
    let waiter = std::thread::spawn({
        let manager_b = manager_b.clone();
        move || {
            let result = manager_b.acquire_manual(
                "manual-b".into(),
                agent_id("agent-b"),
                path("/repo"),
                || {},
            );
            tx.send(result).expect("send waiter result");
        }
    });
    wait_until(|| registry_waiter_count(tempdir.path()).expect("registry") == 1);
    let wake_generation = manager_b.wake_generation();

    assert_eq!(manager_b.release_agent(&agent_id("agent-b")), 0);
    assert!(
        manager_b.wake_generation() > wake_generation,
        "queued-only filesystem release_agent should notify lock waiters"
    );
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(50))
            .expect("waiter should be woken promptly"),
        Err(ManualLockAcquireError::Cancelled)
    ));
    waiter.join().expect("waiter thread");
}

/// Ensures the filesystem backend does not let a blocked waiter for one subtree
/// cause global head-of-line blocking for a later unrelated path request.
#[test]
fn filesystem_backend_blocked_waiter_does_not_block_later_independent_request() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager = filesystem_lock_manager(tempdir.path());
    manager
        .acquire_manual(
            "manual-a".into(),
            agent_id("agent-a"),
            path("/repo/a"),
            || {},
        )
        .expect("blocking manual lock");

    let (first_tx, first_rx) = path_std_sync::mpsc::channel();
    let first = std::thread::spawn({
        let manager = manager.clone();
        move || {
            let result = manager.acquire_manual(
                "manual-b".into(),
                agent_id("agent-b"),
                path("/repo/a/child"),
                || {},
            );
            first_tx.send(result).expect("send first waiter result");
        }
    });
    wait_until(|| registry_waiter_count(tempdir.path()).expect("registry") == 1);

    let (second_tx, second_rx) = path_std_sync::mpsc::channel();
    let second = std::thread::spawn({
        let manager = manager.clone();
        move || {
            let result = manager
                .acquire_auto(
                    "auto-c".into(),
                    agent_id("agent-c"),
                    vec![path("/other")],
                    || {},
                )
                .map(drop);
            second_tx.send(result).expect("send second waiter result");
        }
    });
    second_rx
        .recv()
        .expect("later independent waiter result")
        .expect("later independent waiter should not be blocked globally");
    assert_eq!(
        registry_waiter_count(tempdir.path()).expect("registry"),
        1,
        "only the overlapping waiter should remain queued"
    );

    manager
        .unlock_manual(&agent_id("agent-a"), Path::new("/repo/a"))
        .expect("unlock blocker");
    first_rx
        .recv()
        .expect("overlapping waiter result")
        .expect("overlapping waiter result");
    manager
        .unlock_manual(&agent_id("agent-b"), Path::new("/repo/a/child"))
        .expect("unlock overlapping waiter manual lock");
    first.join().expect("first waiter thread");
    second.join().expect("second waiter thread");
}

/// Ensures the filesystem backend keeps FIFO ordering among overlapping queued
/// waiters even when a later waiter does not overlap the active lock.
#[test]
fn filesystem_backend_overlapping_waiter_stays_behind_earlier_overlapping_waiter() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager = filesystem_lock_manager(tempdir.path());
    manager
        .acquire_manual(
            "manual-a".into(),
            agent_id("agent-a"),
            path("/repo/a"),
            || {},
        )
        .expect("manual lock");

    let (first_tx, first_rx) = path_std_sync::mpsc::channel();
    let first = std::thread::spawn({
        let manager = manager.clone();
        move || {
            let result = manager.acquire_manual(
                "manual-root".into(),
                agent_id("agent-b"),
                path("/repo"),
                || {},
            );
            first_tx.send(result).expect("send first waiter result");
        }
    });
    wait_until(|| registry_waiter_count(tempdir.path()).expect("registry") == 1);

    let (second_tx, second_rx) = path_std_sync::mpsc::channel();
    let second = std::thread::spawn({
        let manager = manager.clone();
        move || {
            let result = manager.acquire_manual(
                "manual-b".into(),
                agent_id("agent-c"),
                path("/repo/b"),
                || {},
            );
            second_tx.send(result).expect("send second waiter result");
        }
    });
    wait_until(|| registry_waiter_count(tempdir.path()).expect("registry") == 2);
    assert_eq!(
        registry_waiter_count(tempdir.path()).expect("registry"),
        2,
        "both overlapping filesystem waiters must remain queued"
    );

    manager
        .unlock_manual(&agent_id("agent-a"), Path::new("/repo/a"))
        .expect("unlock blocker");
    first_rx
        .recv()
        .expect("earlier overlapping waiter result")
        .expect("earlier overlapping waiter result");
    assert_eq!(
        registry_waiter_count(tempdir.path()).expect("registry"),
        1,
        "later overlapping filesystem waiter must remain queued behind the acquired root lock"
    );

    manager
        .unlock_manual(&agent_id("agent-b"), Path::new("/repo"))
        .expect("unlock root");
    second_rx
        .recv()
        .expect("later overlapping waiter result")
        .expect("later overlapping waiter result");
    manager
        .unlock_manual(&agent_id("agent-c"), Path::new("/repo/b"))
        .expect("unlock sibling");
    first.join().expect("first waiter thread");
    second.join().expect("second waiter thread");
}

/// Ensures the filesystem backend revalidates same-owner manual overlaps before
/// granting queued waiters so duplicate manual locks are rejected consistently.
#[test]
fn filesystem_backend_queued_same_owner_manual_waiter_errors_instead_of_duplicate_lock() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager = filesystem_lock_manager(tempdir.path());
    manager
        .acquire_manual(
            "manual-root".into(),
            agent_id("agent-x"),
            path("/repo"),
            || {},
        )
        .expect("blocking manual lock");

    let (first_tx, first_rx) = path_std_sync::mpsc::channel();
    let first = std::thread::spawn({
        let manager = manager.clone();
        move || {
            let result = manager.acquire_manual(
                "manual-a".into(),
                agent_id("agent-a"),
                path("/repo/a"),
                || {},
            );
            first_tx.send(result).expect("send first waiter result");
        }
    });
    wait_until(|| registry_waiter_count(tempdir.path()).expect("registry") == 1);

    let (second_tx, second_rx) = path_std_sync::mpsc::channel();
    let second = std::thread::spawn({
        let manager = manager.clone();
        move || {
            let result = manager.acquire_manual(
                "manual-a-child".into(),
                agent_id("agent-a"),
                path("/repo/a/child"),
                || {},
            );
            second_tx.send(result).expect("send second waiter result");
        }
    });
    wait_until(|| registry_waiter_count(tempdir.path()).expect("registry") == 2);

    manager
        .unlock_manual(&agent_id("agent-x"), Path::new("/repo"))
        .expect("unlock blocker");
    first_rx
        .recv()
        .expect("first same-owner waiter result")
        .expect("first same-owner waiter result");
    assert_eq!(
        second_rx.recv().expect("second same-owner waiter result"),
        Err(ManualLockAcquireError::AlreadyHeld {
            dir: path("/repo/a")
        })
    );

    manager
        .unlock_manual(&agent_id("agent-a"), Path::new("/repo/a"))
        .expect("unlock acquired same-owner lock");
    first.join().expect("first waiter thread");
    second.join().expect("second waiter thread");
}

/// Ensures filesystem backend shutdown removes this instance's registry records
/// so a later manager can acquire the same path without waiting.
#[test]
fn filesystem_backend_shutdown_releases_instance_locks() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    {
        let manager_a = filesystem_lock_manager(tempdir.path());
        manager_a
            .acquire_manual("manual-a".into(), agent_id("agent-a"), path("/repo"), || {})
            .expect("manual lock a");
        manager_a.shutdown();
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

    let removed = manager_b
        .force_unlock_overlapping(Path::new("/repo"))
        .expect("force unlock");
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
    std::fs::set_permissions(tempdir.path(), path_std_fs::Permissions::from_mode(0o755))
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
    std::fs::set_permissions(tempdir.path(), path_std_fs::Permissions::from_mode(0o755))
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

/// Ensures filesystem waiters do not re-check the cross-process registry on a
/// fixed 50 ms cadence forever. The first timed wait remains responsive, but
/// subsequent waits back off while liveness checks keep their own deadline.
#[test]
fn filesystem_wait_polling_uses_adaptive_backoff() {
    let delays = wait_backoff_delays_for_test(8);
    assert_eq!(delays[0], Duration::from_millis(50));
    assert_eq!(delays[1], Duration::from_millis(100));
    assert_eq!(delays[2], Duration::from_millis(200));
    assert!(
        delays.windows(2).all(|pair| pair[0] <= pair[1]),
        "backoff delays should never shrink without an in-process wake"
    );
    assert_eq!(
        *delays.last().expect("delay"),
        Duration::from_secs(1),
        "cross-process polling must cap at the slow backoff ceiling"
    );
}

/// Ensures same-process wake notifications are paired with a generation
/// predicate. A wake observed after the registry check but before timed sleep
/// must skip the full backoff delay instead of losing the notification.
#[test]
fn filesystem_wait_polling_skips_sleep_after_observed_wake() {
    let (elapsed, next_delay) = wait_after_observed_wake_for_test(&DirLockManager::default());
    assert!(
        elapsed < Duration::from_millis(50),
        "observed same-process wake must not sleep for the cross-process backoff: {elapsed:?}"
    );
    assert_eq!(
        next_delay,
        Duration::from_millis(50),
        "same-process wake should reset the next cross-process poll delay"
    );
}

/// Ensures directory-lock wake authority retains its saturating overflow
/// policy, so a max-value wake still leaves waiters observing the same
/// predicate.
#[test]
fn wake_generation_saturates_at_maximum() {
    let mut state = LockState {
        wake_generation: DirLockWakeGeneration::new(u64::MAX),
        ..Default::default()
    };

    state.record_wake();

    assert_eq!(state.wake_generation, DirLockWakeGeneration::new(u64::MAX));
}

/// Ensures the liveness deadline caps the actual timed wait without resetting
/// or preserving the current cross-process backoff step.
#[test]
fn filesystem_wait_polling_liveness_cap_consumes_backoff() {
    let (selected, next_delay) = liveness_cap_consumes_backoff_for_test();
    assert_eq!(
        selected,
        Duration::from_millis(5),
        "the liveness deadline should cap the selected wait duration"
    );
    assert_eq!(
        next_delay,
        Duration::from_secs(1),
        "a liveness-capped timed wait should still consume one max-backoff step"
    );
}

/// Ensures backend reconfiguration fails closed while a memory-backend
/// automatic lock is active, preventing new acquisitions from ignoring the
/// in-flight mutating tool that still releases through the old backend.
#[test]
fn reconfigure_to_filesystem_rejects_active_memory_auto_lock() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let state_dir = tempdir.path().join("state");
    let manager = DirLockManager::default();
    let guard = manager
        .acquire_auto(
            "auto-a".into(),
            agent_id("agent-a"),
            vec![path("/repo")],
            || {},
        )
        .expect("memory automatic lock");

    let error = manager
        .configure(&filesystem_lock_config(&state_dir))
        .expect_err("active auto lock must block backend swap");
    assert!(error.contains("automatic directory lock"));
    drop(guard);
    manager
        .configure(&filesystem_lock_config(&state_dir))
        .expect("backend can switch after active auto drops");
}

/// Ensures backend reconfiguration fails closed while a filesystem-backend
/// automatic lock is active, preserving cross-instance protection until the
/// mutating tool's guard releases its persisted lock.
#[test]
fn reconfigure_from_filesystem_rejects_active_filesystem_auto_lock() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager = filesystem_lock_manager(tempdir.path());
    let guard = manager
        .acquire_auto(
            "auto-a".into(),
            agent_id("agent-a"),
            vec![path("/repo")],
            || {},
        )
        .expect("filesystem automatic lock");

    let error = manager
        .configure(&DirLockConfig {
            enable: true,
            ..DirLockConfig::default()
        })
        .expect_err("active filesystem auto lock must block backend swap");
    assert!(error.contains("filesystem automatic directory lock"));
    drop(guard);
    manager
        .configure(&DirLockConfig {
            enable: true,
            ..DirLockConfig::default()
        })
        .expect("backend can switch after active auto drops");
}

/// Ensures `backend = "filesystem"` remains inert while directory locking is
/// disabled, so merely naming the backend does not create or validate state
/// until `dir_lock.enable = true` opts the feature in.
#[test]
fn disabled_filesystem_backend_does_not_initialize_state_dir() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let state_dir = tempdir.path().join("locks");
    let manager = DirLockManager::default();
    manager
        .configure(&disabled_filesystem_lock_config(&state_dir))
        .expect("disabled filesystem backend should not initialize");
    assert!(
        !state_dir.exists(),
        "disabled filesystem backend should not create state"
    );
}

fn subprocess_helper_env() -> Option<(PathBuf, PathBuf, PathBuf, PathBuf, bool)> {
    let state_dir = std::env::var_os("TAU_DIR_LOCK_SUBPROCESS_STATE").map(PathBuf::from)?;
    let ready = std::env::var_os("TAU_DIR_LOCK_SUBPROCESS_READY").map(PathBuf::from)?;
    let release = std::env::var_os("TAU_DIR_LOCK_SUBPROCESS_RELEASE").map(PathBuf::from)?;
    let lock_dir = std::env::var_os("TAU_DIR_LOCK_SUBPROCESS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| path("/repo"));
    let abort = std::env::var_os("TAU_DIR_LOCK_SUBPROCESS_ABORT").is_some();
    Some((state_dir, ready, release, lock_dir, abort))
}

/// Helper invoked as a separate test process by the subprocess filesystem
/// backend tests; ignored in normal test runs because it is only meaningful
/// when the parent supplies coordination paths through environment variables.
#[test]
#[ignore]
fn filesystem_subprocess_lock_holder_helper() {
    let Some((state_dir, ready, release, lock_dir, abort)) = subprocess_helper_env() else {
        return;
    };
    let manager = filesystem_lock_manager(&state_dir);
    manager
        .acquire_manual(
            "manual-child".into(),
            agent_id("agent-child"),
            lock_dir,
            || {},
        )
        .expect("child manual lock");
    std::fs::write(&ready, b"ready").expect("write ready");
    if abort {
        std::process::abort();
    }
    let start = path_std_time::Instant::now();
    while !release.exists() {
        assert!(start.elapsed() < std::time::Duration::from_secs(120));
        std::thread::sleep(path_std_time::Duration::from_millis(5));
    }
}

fn spawn_lock_holder(
    state_dir: &Path,
    ready: &Path,
    release: &Path,
    abort: bool,
) -> std::process::Child {
    spawn_lock_holder_for_dir(state_dir, ready, release, Path::new("/repo"), abort)
}

fn spawn_lock_holder_for_dir(
    state_dir: &Path,
    ready: &Path,
    release: &Path,
    lock_dir: &Path,
    abort: bool,
) -> std::process::Child {
    let current_exe = std::env::current_exe().expect("current test binary");
    let mut command = path_std_process::Command::new(current_exe);
    command
        .arg("--ignored")
        .arg("--exact")
        .arg("dir_lock::tests::filesystem_subprocess_lock_holder_helper")
        .env("TAU_DIR_LOCK_SUBPROCESS_STATE", state_dir)
        .env("TAU_DIR_LOCK_SUBPROCESS_READY", ready)
        .env("TAU_DIR_LOCK_SUBPROCESS_RELEASE", release)
        .env("TAU_DIR_LOCK_SUBPROCESS_DIR", lock_dir);
    if abort {
        command.env("TAU_DIR_LOCK_SUBPROCESS_ABORT", "1");
    }
    command.spawn().expect("spawn lock holder")
}

/// Ensures the filesystem backend really coordinates across OS processes, not
/// only across multiple managers inside one Rust process.
#[test]
fn filesystem_backend_blocks_real_subprocess_until_exit() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let state_dir = tempdir.path().join("state");
    let ready = tempdir.path().join("ready");
    let release = tempdir.path().join("release");
    let mut child = spawn_lock_holder(&state_dir, &ready, &release, false);
    wait_until(|| ready.exists());

    let manager = filesystem_lock_manager(&state_dir);
    let waiter = std::thread::spawn({
        let manager = manager.clone();
        move || {
            manager.acquire_manual_with_policy(
                "manual-parent".into(),
                agent_id("agent-parent"),
                path("/repo"),
                || {},
                LockWaitPolicy {
                    liveness_interval: Duration::from_millis(25),
                    abandoned_after: Duration::from_secs(60),
                },
            )
        }
    });
    std::thread::sleep(Duration::from_millis(150));
    assert!(!waiter.is_finished(), "subprocess lease must block parent");
    std::fs::write(&release, b"release").expect("release child");
    child.wait().expect("child exits");
    waiter.join().expect("waiter").expect("parent acquired");
}

/// Ensures an abnormal process exit releases the OS lease and lets a peer reap
/// the abandoned registry record before acquiring the same directory.
#[test]
fn filesystem_backend_reaps_abnormally_exited_subprocess() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let state_dir = tempdir.path().join("state");
    let ready = tempdir.path().join("ready");
    let release = tempdir.path().join("release");
    let mut child = spawn_lock_holder(&state_dir, &ready, &release, true);
    wait_until(|| ready.exists());
    let _ = child.wait().expect("child abort status");

    let manager = filesystem_lock_manager(&state_dir);
    manager
        .acquire_manual(
            "manual-parent".into(),
            agent_id("agent-parent"),
            path("/repo"),
            || panic!("dead subprocess should be reaped before waiting"),
        )
        .expect("parent acquired after reap");
}

/// Ensures uncertain lease-liveness errors fail closed: if a peer lease exists
/// but cannot be opened for liveness probing, the filesystem backend reports an
/// explicit backend error instead of reaping the peer and granting a
/// conflicting lock.
#[cfg(unix)]
#[test]
fn filesystem_backend_preserves_peer_record_on_lease_open_error() {
    use std::os::unix::fs::PermissionsExt;

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let state_dir = tempdir.path().join("state");
    let ready = tempdir.path().join("ready");
    let release = tempdir.path().join("release");
    let mut child =
        spawn_lock_holder_for_dir(&state_dir, &ready, &release, Path::new("/unrelated"), false);
    wait_until(|| ready.exists());

    let manager = filesystem_lock_manager(&state_dir);
    let instances_dir = state_dir.join("instances");
    let original_permissions = std::fs::metadata(&instances_dir)
        .expect("instances metadata")
        .permissions();
    std::fs::set_permissions(&instances_dir, path_std_fs::Permissions::from_mode(0o000))
        .expect("hide instances dir");
    let result = manager.acquire_manual(
        "manual-parent".into(),
        agent_id("agent-parent"),
        path("/repo"),
        || panic!("uncertain lease liveness should not wait or acquire"),
    );
    assert!(matches!(result, Err(ManualLockAcquireError::Backend(_))));

    std::fs::set_permissions(&instances_dir, original_permissions).expect("restore instances dir");
    std::fs::write(&release, b"release").expect("release child");
    child.wait().expect("child exits");
}

/// Ensures backend-error exits after a filesystem waiter has been persisted
/// remove that waiter best-effort, so the live owning instance does not leave a
/// stale FIFO entry that blocks unrelated later acquisitions.
#[cfg(unix)]
#[test]
fn filesystem_backend_error_after_enqueue_cleans_up_waiter() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager_a = filesystem_lock_manager(tempdir.path());
    let manager_b = filesystem_lock_manager(tempdir.path());
    manager_a
        .acquire_manual("manual-a".into(), agent_id("agent-a"), path("/repo"), || {})
        .expect("blocking manual lock");

    let waiter = std::thread::spawn({
        let manager_b = manager_b.clone();
        move || {
            manager_b.acquire_manual_with_policy(
                "manual-b".into(),
                agent_id("agent-b"),
                path("/repo"),
                || {},
                LockWaitPolicy {
                    liveness_interval: Duration::from_millis(25),
                    abandoned_after: Duration::from_secs(60),
                },
            )
        }
    });
    wait_until(|| registry_waiter_count(tempdir.path()).expect("registry") == 1);

    set_fail_reap_for_test(Some(tempdir.path().to_path_buf()));
    assert!(matches!(
        waiter.join().expect("waiter"),
        Err(ManualLockAcquireError::Backend(_))
    ));
    set_fail_reap_for_test(None);
    assert_eq!(
        registry_waiter_count(tempdir.path()).expect("registry"),
        0,
        "backend-error cleanup should remove the failed call's waiter"
    );

    let manager_c = filesystem_lock_manager(tempdir.path());
    manager_c
        .acquire_manual(
            "manual-c".into(),
            agent_id("agent-c"),
            path("/other"),
            || panic!("unrelated later acquisition must not wait behind a stale waiter"),
        )
        .expect("unrelated later acquisition must not be FIFO-blocked by stale waiter");
}

/// Ensures a queued filesystem automatic waiter cannot become an invisible old
/// backend lock while a backend reconfiguration is paused after its active-lock
/// check. The waiter must re-enter the backend admission gate before granting;
/// if the configure wins, old-backend shutdown cancels the queued waiter.
#[test]
fn filesystem_queued_auto_grant_serializes_with_backend_reconfigure() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let manager = filesystem_lock_manager(tempdir.path());
    manager
        .acquire_manual(
            "manual-blocker".into(),
            agent_id("agent-blocker"),
            path("/repo"),
            || {},
        )
        .expect("blocking manual lock");

    let waiter = std::thread::spawn({
        let manager = manager.clone();
        move || {
            manager.acquire_auto(
                "auto-waiter".into(),
                agent_id("agent-waiter"),
                vec![path("/repo")],
                || {},
            )
        }
    });
    wait_until(|| registry_waiter_count(tempdir.path()).expect("registry") == 1);

    let pause = install_configure_pause_for_test();
    let configure = std::thread::spawn({
        let manager = manager.clone();
        move || {
            manager.configure(&DirLockConfig {
                enable: true,
                backend: DirLockBackendConfig::Memory,
                state_dir: None,
                enforce_ro_bind: true,
            })
        }
    });
    pause.wait_until_reached();

    manager
        .unlock_manual(&agent_id("agent-blocker"), Path::new("/repo"))
        .expect("release blocker");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !waiter.is_finished(),
        "queued waiter must block on backend admission while configure is paused"
    );

    pause.release();
    clear_configure_pause_for_test();
    configure.join().expect("configure").expect("configure");
    assert!(matches!(
        waiter.join().expect("waiter"),
        Err(LockAcquireError::Cancelled)
    ));

    let guard = manager
        .acquire_auto(
            "auto-memory".into(),
            agent_id("agent-memory"),
            vec![path("/repo")],
            || {},
        )
        .expect("new memory backend auto lock");
    drop(guard);
}
