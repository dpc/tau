use std::os::unix::net::UnixListener;
use std::process::{Child, Command};

use tempfile::TempDir;

use super::*;

fn runtime_override(temp: &TempDir) -> RuntimeDirOverride {
    override_test_runtime_dir(temp.path())
}

fn discover_for_test(
    query: Option<&str>,
    limit: usize,
    current_session_id: &str,
) -> PeerSessionSnapshot {
    discover_peer_sessions_for_test(query, limit, current_session_id)
}

fn write_peer_metadata(path: &Path, session_id: &str, project_root: &Path, opted_in: bool) {
    std::fs::write(
        metadata_path(path),
        serde_json::to_vec(&DaemonMetadata {
            version: DAEMON_METADATA_VERSION,
            pid: std::process::id(),
            project_root: Some(project_root.to_path_buf()),
            session_id: session_id.to_owned(),
            peer_entrypoint: opted_in,
        })
        .expect("metadata json"),
    )
    .expect("write metadata");
}

/// Attach metadata is untrusted discovery input; an invalid controlled
/// session identifier must fail closed before the CLI stores it.
#[test]
fn read_session_id_rejects_invalid_runtime_metadata() {
    let temp = tempfile::tempdir().expect("tempdir");
    let harness_path = temp.path().join("harness");
    write_peer_metadata(&harness_path, "bad.id", temp.path(), false);

    let error = read_session_id(&harness_path).expect_err("invalid id must fail closed");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("invalid daemon session id"));
}

/// Metadata readers preserve missing and malformed diagnostics instead of
/// collapsing both into an absent session id.
#[test]
fn read_session_id_distinguishes_missing_and_malformed_metadata() {
    let temp = tempfile::tempdir().expect("tempdir");
    let harness_path = temp.path().join("harness");

    let missing = read_session_id(&harness_path).expect_err("missing metadata");
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);

    std::fs::write(metadata_path(&harness_path), b"{").expect("malformed metadata");
    let malformed = read_session_id(&harness_path).expect_err("malformed metadata");
    assert_eq!(malformed.kind(), std::io::ErrorKind::InvalidData);
    assert!(malformed.to_string().contains("malformed daemon metadata"));
}

fn spawn_probe_daemon(
    path: &Path,
    expected_session: &str,
    requests: usize,
    respond: bool,
) -> std::thread::JoinHandle<()> {
    let listener = tau_socket::SocketListener::bind(socket_path(path)).expect("probe listener");
    let expected_session = expected_session.to_owned();
    std::thread::spawn(move || {
        for _ in 0..requests {
            let mut client = listener.accept().expect("accept probe");
            assert!(matches!(
                client.recv().expect("hello"),
                Some(tau_proto::HarnessInputMessage::Hello(_))
            ));
            let Some(tau_proto::HarnessInputMessage::PeerSessionProbe(probe)) =
                client.recv().expect("probe")
            else {
                panic!("expected peer session probe");
            };
            assert_eq!(probe.session_id, expected_session);
            if respond {
                client
                    .send(&tau_proto::HarnessOutputMessage::PeerSessionProbeResult(
                        tau_proto::PeerSessionProbeResult {
                            request_id: probe.request_id,
                            available: true,
                        },
                    ))
                    .expect("probe result");
            } else {
                std::thread::sleep(SESSION_DISCOVERY_PROBE_TIMEOUT * 2);
            }
        }
    })
}

fn spawn_current_session_daemon(path: &Path, session_id: &str) -> std::thread::JoinHandle<()> {
    let listener =
        tau_socket::SocketListener::bind(socket_path(path)).expect("current-session listener");
    let session_id =
        tau_proto::SessionId::parse(session_id).expect("known-safe SessionId must be valid");
    let project_root = path
        .parent()
        .expect("runtime path parent")
        .canonicalize()
        .expect("canonical project root");
    std::thread::spawn(move || {
        let mut client = listener.accept().expect("accept current-session probe");
        assert!(matches!(
            client.recv().expect("hello"),
            Some(tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
                client_kind: tau_proto::ClientKind::Ui,
                ..
            }))
        ));
        let Some(tau_proto::HarnessInputMessage::GetCurrentSession(request)) =
            client.recv().expect("current-session request")
        else {
            panic!("expected current-session request");
        };
        client
            .send(&tau_proto::HarnessOutputMessage::CurrentSessionResult(
                tau_proto::CurrentSessionResult {
                    request_id: "unrelated-request".to_owned(),
                    session_id: "wrong-session"
                        .parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                    project_root: PathBuf::from("/wrong/project"),
                },
            ))
            .expect("unrelated current-session result");
        client
            .send(&tau_proto::HarnessOutputMessage::CurrentSessionResult(
                tau_proto::CurrentSessionResult {
                    request_id: request.request_id,
                    session_id,
                    project_root,
                },
            ))
            .expect("current-session result");
    })
}

/// Exercises live opt-in confirmation against two daemon sockets while
/// proving a non-opted daemon is not probed and full project paths are
/// never returned.
#[test]
fn peer_discovery_two_daemons_requires_opt_in_and_redacts_project_root() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    let opted = harnesses_dir().join("a");
    let private = harnesses_dir().join("b");
    let opted_daemon = spawn_probe_daemon(&opted, "live-session", 1, true);
    let _private_listener =
        tau_socket::SocketListener::bind(socket_path(&private)).expect("private listener");
    write_peer_metadata(
        &opted,
        "live-session",
        Path::new("/secret/parent/public-project"),
        true,
    );
    write_peer_metadata(
        &private,
        "private-session",
        Path::new("/secret/parent/private-project"),
        false,
    );

    let snapshot = discover_for_test(None, 50, "live-session");

    assert_eq!(
        snapshot.sessions,
        vec![PeerSession {
            session_id: "live-session".to_owned(),
            project_label: Some("public-project".to_owned()),
            current: true,
        }]
    );
    assert!(!format!("{snapshot:?}").contains("/secret/"));
    opted_daemon.join().expect("opted daemon");
}

/// Ensures a daemon's rewritten active session is authoritative and stale
/// metadata is omitted from the same discovery snapshot.
#[test]
fn peer_discovery_tracks_active_session_and_omits_stale_record() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    let live = harnesses_dir().join("live");
    let stale = harnesses_dir().join("stale");
    let daemon = spawn_probe_daemon(&live, "new-session", 1, true);
    write_peer_metadata(&live, "old-session", Path::new("/project"), true);
    update_session_id(&live, "new-session").expect("rewrite active session");
    write_peer_metadata(&stale, "stale-session", Path::new("/stale"), true);
    std::fs::write(socket_path(&stale), b"not a socket").expect("stale socket");

    let snapshot = discover_for_test(None, 50, "");

    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].session_id, "new-session");
    daemon.join().expect("live daemon");
}

/// Ensures two live daemons claiming one routing key are conservatively
/// omitted instead of exposing a nondeterministic peer destination.
#[test]
fn peer_discovery_omits_ambiguous_live_session() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    let first = harnesses_dir().join("first");
    let second = harnesses_dir().join("second");
    let first_daemon = spawn_probe_daemon(&first, "same", 1, true);
    let second_daemon = spawn_probe_daemon(&second, "same", 1, true);
    write_peer_metadata(&first, "same", Path::new("/one"), true);
    write_peer_metadata(&second, "same", Path::new("/two"), true);

    assert!(discover_for_test(None, 50, "").sessions.is_empty());

    first_daemon.join().expect("first daemon");
    second_daemon.join().expect("second daemon");
}

/// Proves traversal counts every directory entry before filtering, so a
/// directory dominated by unrelated files cannot cause unbounded scanning.
#[test]
fn peer_discovery_counts_non_candidates_toward_scan_bound() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    for index in 0..=SESSION_DISCOVERY_MAX_CANDIDATES {
        std::fs::write(harnesses_dir().join(format!("{index}.unrelated")), b"x")
            .expect("unrelated entry");
    }

    let snapshot = discover_for_test(None, 50, "");

    assert!(snapshot.scan_truncated);
    assert!(snapshot.sessions.is_empty());
}

/// Cancellation raised by one directory dequeue is observed before the
/// iterator can perform another filesystem dequeue.
#[test]
fn bounded_directory_iteration_stops_before_post_cancel_dequeue() {
    let cancelled = AtomicBool::new(false);
    let dequeues = AtomicUsize::new(0);
    let entries = std::iter::from_fn(|| {
        dequeues.fetch_add(1, Ordering::AcqRel);
        cancelled.store(true, Ordering::Release);
        Some(())
    });

    assert!(
        collect_directory_entries_bounded(
            entries,
            Instant::now() + Duration::from_secs(1),
            &cancelled
        )
        .is_err()
    );
    assert_eq!(dequeues.load(Ordering::Acquire), 1);
}

/// Whole-call admission rejects a ninth concurrent coordinator without
/// spawning or queueing and restores every slot after completion.
#[test]
fn peer_discovery_whole_call_admission_is_non_queued() {
    let _serial = TEST_DISCOVERY_SERIAL
        .lock()
        .expect("test discovery serial lock poisoned");
    let wait_deadline = Instant::now() + Duration::from_secs(1);
    while ACTIVE_DISCOVERY_CALLS.load(Ordering::Acquire) != 0 {
        assert!(
            Instant::now() < wait_deadline,
            "discovery lease did not retire"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let permits = (0..SESSION_DISCOVERY_MAX_CALLS)
        .map(|_| DiscoveryCallPermit::try_acquire().expect("discovery call slot"))
        .collect::<Vec<_>>();
    assert!(DiscoveryCallPermit::try_acquire().is_none());
    drop(permits);
    assert!(DiscoveryCallPermit::try_acquire().is_some());
}

/// A stalled storage scan cannot retain the caller beyond the total
/// deadline; its bounded lease remains charged until the isolated
/// worker exits.
#[test]
fn peer_discovery_slow_storage_isolated_by_total_deadline() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    *TEST_DISCOVERY_SCAN_DELAY
        .lock()
        .expect("test scan delay lock poisoned") = Some((harnesses_dir(), 2_100));
    let _serial = TEST_DISCOVERY_SERIAL
        .lock()
        .expect("test discovery serial lock poisoned");
    let started = Instant::now();

    let snapshot = discover_peer_sessions(
        None,
        50,
        "",
        DiscoveryCallPermit::try_acquire().expect("discovery call permit"),
    );

    *TEST_DISCOVERY_SCAN_DELAY
        .lock()
        .expect("test scan delay lock poisoned") = None;
    assert!(started.elapsed() < Duration::from_millis(2_200));
    assert!(snapshot.sessions.is_empty());
    assert!(snapshot.scan_truncated);
    std::thread::sleep(Duration::from_millis(150));
}

/// Proves the metadata reader accepts the exact 16 KiB boundary and rejects
/// one additional byte without parsing or allocating an unbounded payload.
#[test]
fn peer_discovery_metadata_byte_limit_is_inclusive_and_bounded() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    let path = harnesses_dir().join("metadata-boundary");
    let base = serde_json::to_vec(&DaemonMetadata {
        version: DAEMON_METADATA_VERSION,
        pid: std::process::id(),
        project_root: None,
        session_id: "boundary".to_owned(),
        peer_entrypoint: true,
    })
    .expect("metadata");
    let mut exact = base.clone();
    exact.resize(SESSION_DISCOVERY_MAX_METADATA_BYTES as usize, b' ');
    std::fs::write(metadata_path(&path), &exact).expect("exact metadata");
    let cancelled = AtomicBool::new(false);
    assert!(
        read_metadata_bounded(&path, Instant::now() + Duration::from_secs(1), &cancelled).is_some()
    );

    exact.push(b' ');
    std::fs::write(metadata_path(&path), exact).expect("oversized metadata");
    assert!(
        read_metadata_bounded(&path, Instant::now() + Duration::from_secs(1), &cancelled).is_none()
    );
}

/// Proves result truncation remains explicit after live confirmation.
#[test]
fn peer_discovery_reports_result_truncation() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    let count = SESSION_DISCOVERY_MAX_RESULTS + 1;
    let mut daemons = Vec::new();
    for index in 0..count {
        let path = harnesses_dir().join(format!("{index:03}"));
        let session = format!("session-{index:03}");
        daemons.push(spawn_probe_daemon(&path, &session, 1, true));
        write_peer_metadata(&path, &session, Path::new("/project"), true);
    }

    let snapshot = discover_for_test(None, SESSION_DISCOVERY_MAX_RESULTS, "");

    assert_eq!(snapshot.sessions.len(), SESSION_DISCOVERY_MAX_RESULTS);
    assert!(snapshot.truncated);
    for daemon in daemons {
        daemon.join().expect("probe daemon");
    }
}

/// Repeated timed-out probes must return only after scoped workers exit,
/// preventing deadline cancellation from accumulating background workers.
#[test]
fn peer_discovery_deadline_cancellation_leaves_no_workers() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    let calls = 3;
    let mut daemons = Vec::new();
    for index in 0..(SESSION_DISCOVERY_MAX_PROBES + 1) {
        let path = harnesses_dir().join(format!("blocked-{index}"));
        let session = format!("blocked-session-{index}");
        daemons.push(spawn_probe_daemon(&path, &session, calls, false));
        write_peer_metadata(&path, &session, Path::new("/project"), true);
    }

    for _ in 0..calls {
        assert!(discover_for_test(None, 50, "").sessions.is_empty());
        assert_eq!(ACTIVE_DISCOVERY_WORKERS.load(Ordering::Acquire), 0);
    }
    for daemon in daemons {
        daemon.join().expect("blocked daemon");
    }
}

/// Saturated global probe slots keep candidates queued through the total
/// deadline; cancellation prevents a later socket connect after return.
#[test]
fn peer_discovery_total_deadline_cancels_queued_probe_before_io() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    let path = harnesses_dir().join("queued");
    let listener = UnixListener::bind(socket_path(&path)).expect("queued listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    write_peer_metadata(&path, "queued-session", Path::new("/project"), true);
    let _serial = TEST_DISCOVERY_SERIAL
        .lock()
        .expect("test discovery serial lock poisoned");
    let cancelled = AtomicBool::new(false);
    let slots = (0..SESSION_DISCOVERY_MAX_PROBES)
        .map(|_| {
            DiscoveryProbeSlot::acquire(Instant::now() + Duration::from_secs(5), &cancelled)
                .expect("probe slot")
        })
        .collect::<Vec<_>>();
    let started = Instant::now();

    let snapshot = discover_peer_sessions(
        None,
        50,
        "",
        DiscoveryCallPermit::try_acquire().expect("discovery call permit"),
    );

    assert!(started.elapsed() < Duration::from_millis(2_200));
    assert!(snapshot.sessions.is_empty());
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert_eq!(ACTIVE_DISCOVERY_WORKERS.load(Ordering::Acquire), 0);
    drop(slots);
}

/// Ensures daemon metadata's `session_id` field is the active session and
/// can be updated in place without losing the stable pid/project fields.
#[test]
fn update_session_id_rewrites_active_session_metadata() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("project root");
    let paths = prepare_harness_paths(&project_root, "old-session").expect("paths");
    paths.write_metadata().expect("write metadata");

    update_session_id(paths.path(), "new-session").expect("update session");

    let metadata = read_metadata(paths.path()).expect("metadata");
    assert_eq!(metadata.session_id, "new-session");
    assert_eq!(metadata.pid, std::process::id());
    assert_eq!(
        metadata.project_root.as_deref(),
        Some(project_root.as_path())
    );
}

/// Runtime metadata writers reject identifiers their reader cannot accept.
#[test]
fn runtime_metadata_writers_reject_invalid_session_ids() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("project root");

    let prepare_error = match prepare_harness_paths(&project_root, "bad.id") {
        Ok(_) => panic!("invalid initial id must fail"),
        Err(error) => error,
    };
    assert_eq!(prepare_error.kind(), std::io::ErrorKind::InvalidInput);

    let paths = prepare_harness_paths(&project_root, "valid").expect("paths");
    paths.write_metadata().expect("write metadata");
    let update_error = update_session_id(paths.path(), "bad.id").expect_err("invalid updated id");
    assert_eq!(update_error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        read_session_id(paths.path())
            .expect("unchanged valid id")
            .as_str(),
        "valid"
    );
}

/// Models two daemons that report the same PID from separate PID namespaces
/// and ensures their shared runtime directory still receives distinct
/// sockets.
#[test]
fn process_instance_ids_disambiguate_equal_pid_socket_paths() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("project root");
    let first_instance = HarnessInstanceId::parse("0000000000000001").expect("first instance id");
    let second_instance = HarnessInstanceId::parse("0000000000000002").expect("second instance id");

    let first = prepare_harness_paths_for_instance(&project_root, "first", &first_instance)
        .expect("first paths");
    let second = prepare_harness_paths_for_instance(&project_root, "second", &second_instance)
        .expect("second paths");
    let _first_listener = UnixListener::bind(first.socket_path()).expect("first same-PID socket");
    let _second_listener =
        UnixListener::bind(second.socket_path()).expect("second same-PID socket");

    assert_ne!(first.path(), second.path());
    assert!(
        first
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&format!("{}-", std::process::id())))
    );
}

/// Ensures running-session listing derives lifecycle from responsive daemon
/// memory rather than persisted sessions, runtime metadata, or stale paths.
#[test]
fn running_session_list_includes_only_reachable_active_sessions() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    let live = harnesses_dir().join("live");
    let stale = harnesses_dir().join(std::process::id().to_string());
    let daemon = spawn_current_session_daemon(&live, "running-session");
    write_peer_metadata(&stale, "historical-session", Path::new("/stale"), false);
    std::fs::write(socket_path(&stale), b"not a socket").expect("stale socket");

    assert_eq!(
        list_running_sessions().expect("running sessions"),
        vec![RunningSession {
            session_id: "running-session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            project_root: harnesses_dir().canonicalize().expect("canonical root"),
        }]
    );
    daemon.join().expect("current-session daemon");
}

/// Ensures absence of runtime candidates is a successful empty,
/// pipe-friendly listing rather than a synthesized placeholder row.
#[test]
fn running_session_list_is_empty_without_runtime_directory() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);

    assert!(
        list_running_sessions()
            .expect("running sessions")
            .is_empty()
    );
    assert!(!harnesses_dir().exists());
}

/// Ensures daemon memory remains authoritative when adjacent metadata is
/// missing or unreadable during a rewrite or startup window.
#[test]
fn running_session_list_ignores_invalid_runtime_metadata() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    let live = harnesses_dir().join("live");
    let daemon = spawn_current_session_daemon(&live, "authoritative-session");
    std::fs::write(metadata_path(&live), b"{").expect("invalid metadata");

    assert_eq!(
        list_running_sessions().expect("running sessions"),
        vec![RunningSession {
            session_id: "authoritative-session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            project_root: harnesses_dir().canonicalize().expect("canonical root"),
        }]
    );
    daemon.join().expect("current-session daemon");
}

/// Ensures listing sorts responsive records while retaining
/// indistinguishable identities so callers can detect multiple
/// harnesses for one directory.
#[test]
fn running_session_list_sorts_and_retains_duplicate_identities() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    let z = spawn_current_session_daemon(&harnesses_dir().join("a"), "z-session");
    let duplicate = spawn_current_session_daemon(&harnesses_dir().join("b"), "a-session");
    let a = spawn_current_session_daemon(&harnesses_dir().join("c"), "a-session");

    assert_eq!(
        list_running_sessions().expect("running sessions"),
        vec![
            RunningSession {
                session_id: "a-session"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                project_root: harnesses_dir().canonicalize().expect("canonical root"),
            },
            RunningSession {
                session_id: "a-session"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                project_root: harnesses_dir().canonicalize().expect("canonical root"),
            },
            RunningSession {
                session_id: "z-session"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                project_root: harnesses_dir().canonicalize().expect("canonical root"),
            },
        ]
    );
    z.join().expect("z daemon");
    duplicate.join().expect("duplicate daemon");
    a.join().expect("a daemon");
}

/// Ensures the raw directory-entry bound fails the whole listing rather
/// than returning a partial set after a stale-file flood.
#[test]
fn running_session_list_fails_at_directory_entry_bound() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    for index in 0..=SESSION_LOOKUP_MAX_DIRECTORY_ENTRIES {
        std::fs::write(harnesses_dir().join(format!("junk-{index}")), b"junk")
            .expect("junk runtime entry");
    }

    let error = list_running_sessions().expect_err("bounded listing");
    assert!(
        error
            .to_string()
            .contains("runtime directory entry limit reached")
    );
}

/// Ensures one responsive non-Tau or stalled socket consumes only its
/// per-probe budget and cannot hide a later responsive harness.
#[test]
fn running_session_list_continues_after_one_probe_timeout() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    let blocked = harnesses_dir().join("a-blocked");
    let live = harnesses_dir().join("b-live");
    let blocked_listener =
        tau_socket::SocketListener::bind(socket_path(&blocked)).expect("blocked listener");
    let blocked_daemon = std::thread::spawn(move || {
        let _client = blocked_listener.accept().expect("accept blocked probe");
        std::thread::sleep(SESSION_DISCOVERY_PROBE_TIMEOUT * 2);
    });
    let live_daemon = spawn_current_session_daemon(&live, "responsive-session");
    let started = Instant::now();

    assert_eq!(
        list_running_sessions().expect("running sessions"),
        vec![RunningSession {
            session_id: "responsive-session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            project_root: harnesses_dir().canonicalize().expect("canonical root"),
        }]
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    blocked_daemon.join().expect("blocked daemon");
    live_daemon.join().expect("live daemon");
}

/// Ensures a blocked runtime-directory operation is isolated from the
/// caller's total deadline while retaining bounded discovery admission.
#[test]
fn running_session_list_isolates_slow_storage() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    let _serial = TEST_DISCOVERY_SERIAL
        .lock()
        .expect("test discovery serial lock poisoned");
    *TEST_DISCOVERY_SCAN_DELAY
        .lock()
        .expect("test scan delay lock poisoned") = Some((harnesses_dir(), 2_100));
    let started = Instant::now();

    let error = list_running_sessions().expect_err("scan deadline");

    *TEST_DISCOVERY_SCAN_DELAY
        .lock()
        .expect("test scan delay lock poisoned") = None;
    assert!(started.elapsed() < Duration::from_millis(2_200));
    assert!(error.to_string().contains("runtime scan timed out"));
}

/// Ensures expiry inside the final probe fails the whole listing rather
/// than returning the ids collected before the global budget ran out.
#[test]
fn running_session_list_rejects_partial_result_on_final_probe_deadline() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
    let _serial = TEST_DISCOVERY_SERIAL
        .lock()
        .expect("test discovery serial lock poisoned");
    let live = spawn_current_session_daemon(&harnesses_dir().join("a-live"), "collected");
    let blocked = harnesses_dir().join("z-blocked");
    let listener =
        tau_socket::SocketListener::bind(socket_path(&blocked)).expect("blocked listener");
    let daemon = std::thread::spawn(move || {
        let _client = listener.accept().expect("accept final probe");
        std::thread::sleep(SESSION_DISCOVERY_PROBE_TIMEOUT);
    });
    *TEST_DISCOVERY_SCAN_DELAY
        .lock()
        .expect("test scan delay lock poisoned") = Some((harnesses_dir(), 1_750));

    let error = list_running_sessions().expect_err("global probe deadline");

    *TEST_DISCOVERY_SCAN_DELAY
        .lock()
        .expect("test scan delay lock poisoned") = None;
    assert!(error.to_string().contains("probe deadline expired"));
    live.join().expect("live daemon");
    daemon.join().expect("blocked daemon");
}

/// Ensures session discovery ignores the old active-session value after a
/// metadata rewrite and still finds the same live socket under the new id.
#[test]
fn find_harness_for_session_tracks_updated_active_session() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("project root");
    let paths = prepare_harness_paths(&project_root, "old-session").expect("paths");
    let _listener = UnixListener::bind(paths.socket_path()).expect("socket");
    paths.write_metadata().expect("write metadata");

    update_session_id(paths.path(), "new-session").expect("update session");

    assert_eq!(
        find_harness_for_session("old-session").expect("old lookup"),
        None
    );
    assert_eq!(
        find_harness_for_session("new-session").expect("new lookup"),
        Some(paths.path().to_path_buf())
    );
}

/// A dead unreachable claimant is ignored but preserved because discovery
/// cannot atomically exclude a concurrent PID-reuse/startup replacement.
#[cfg(target_os = "linux")]
#[test]
fn find_harness_for_session_preserves_dead_socket() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("project root");
    let paths = prepare_harness_paths(&project_root, "session").expect("paths");
    write_metadata_with_pid(paths.path(), &project_root, "session", dead_pid());
    std::fs::write(paths.socket_path(), b"not a listener").expect("stale socket marker");

    assert_eq!(find_harness_for_session("session").expect("lookup"), None);
    assert!(metadata_path(paths.path()).exists());
    assert!(paths.socket_path().exists());
}

/// Ensures a transient connection failure cannot unlink runtime metadata
/// for a harness whose process still exists. This protects
/// external-message discovery from turning one failed liveness probe
/// into a permanent "no running daemon" failure for a live session.
#[test]
fn find_harness_for_session_keeps_files_when_pid_is_alive() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let child = ChildGuard::new();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("project root");
    let paths = prepare_harness_paths(&project_root, "session").expect("paths");
    write_metadata_with_pid(paths.path(), &project_root, "session", child.id());
    std::fs::write(paths.socket_path(), b"not a listener").expect("stale socket marker");

    assert!(matches!(
        find_harness_for_session("session"),
        Err(FindHarnessForSessionError::Incomplete { .. })
    ));
    assert!(metadata_path(paths.path()).exists());
    assert!(paths.socket_path().exists());
}

/// Hundreds of unrelated stale lifecycle pairs must not hide one live
/// claimant merely because they exceed the general discovery candidate cap.
#[cfg(target_os = "linux")]
#[test]
fn find_harness_for_session_crosses_unrelated_stale_flood() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let dir = harnesses_dir();
    std::fs::create_dir_all(&dir).expect("harnesses dir");
    let pid_max: u32 = std::fs::read_to_string("/proc/sys/kernel/pid_max")
        .expect("pid max")
        .trim()
        .parse()
        .expect("numeric pid max");
    for offset in 1..=300 {
        let pid = pid_max + offset;
        let path = dir.join(pid.to_string());
        drop(UnixListener::bind(socket_path(&path)).expect("stale socket"));
        write_metadata_with_pid(&path, temp.path(), "unrelated-session", pid);
    }
    let live = dir.join("live-target");
    let _listener = UnixListener::bind(socket_path(&live)).expect("live socket");
    write_peer_metadata(&live, "live-session", temp.path(), true);

    assert_eq!(
        find_harness_for_session("live-session").expect("bounded lookup"),
        Some(live)
    );
    assert_eq!(
        std::fs::read_dir(&dir).expect("runtime entries").count(),
        602,
        "lookup must remain non-destructive"
    );
}

/// More session-matching records than the candidate budget keep uniqueness
/// incomplete even when one early claimant is live.
#[cfg(target_os = "linux")]
#[test]
fn find_harness_for_session_fails_closed_at_matching_candidate_bound() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let dir = harnesses_dir();
    std::fs::create_dir_all(&dir).expect("harnesses dir");
    let live = dir.join("live-target");
    let _listener = UnixListener::bind(socket_path(&live)).expect("live socket");
    write_peer_metadata(&live, "bounded-session", temp.path(), true);
    let pid_max: u32 = std::fs::read_to_string("/proc/sys/kernel/pid_max")
        .expect("pid max")
        .trim()
        .parse()
        .expect("numeric pid max");
    for offset in 1..=SESSION_DISCOVERY_MAX_CANDIDATES as u32 {
        let pid = pid_max + offset;
        let path = dir.join(pid.to_string());
        drop(UnixListener::bind(socket_path(&path)).expect("stale socket"));
        write_metadata_with_pid(&path, temp.path(), "bounded-session", pid);
    }

    assert!(matches!(
        find_harness_for_session("bounded-session"),
        Err(FindHarnessForSessionError::Incomplete { .. })
    ));
    assert_eq!(
        std::fs::read_dir(&dir).expect("runtime entries").count(),
        (SESSION_DISCOVERY_MAX_CANDIDATES + 1) * 2
    );
}

/// Cancellation that arrives after parsing the final unrelated metadata
/// record must still prevent a false complete `None` result.
#[test]
fn find_harness_for_session_checks_cancellation_after_metadata_read() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let dir = harnesses_dir();
    std::fs::create_dir_all(&dir).expect("harnesses dir");
    let unrelated = dir.join("unrelated");
    write_peer_metadata(&unrelated, "other-session", temp.path(), true);
    TEST_CANCEL_AFTER_SESSION_METADATA_READ.with(|enabled| enabled.set(true));

    assert!(matches!(
        find_harness_for_session_until(
            "missing-session",
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(false)
        ),
        Err(FindHarnessForSessionError::Incomplete { .. })
    ));
}

/// Project and session discovery are both non-destructive: a failed probe
/// cannot destroy live daemon markers during a transient failure.
#[test]
fn find_harness_for_dir_keeps_files_when_pid_is_alive() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let child = ChildGuard::new();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("project root");
    let paths = prepare_harness_paths(&project_root, "session").expect("paths");
    write_metadata_with_pid(paths.path(), &project_root, "session", child.id());
    std::fs::write(paths.socket_path(), b"not a listener").expect("stale socket marker");

    assert_eq!(find_harness_for_dir(&project_root), None);
    assert!(metadata_path(paths.path()).exists());
    assert!(paths.socket_path().exists());
}

/// Ensures discovery reports ambiguity when two live harnesses advertise
/// the same active session, preventing nondeterministic cross-harness
/// sends.
#[test]
fn find_harness_for_session_reports_ambiguity() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let dir = harnesses_dir();
    std::fs::create_dir_all(&dir).expect("harnesses dir");
    let path_a = dir.join("a");
    let path_b = dir.join("b");
    let _listener_a = UnixListener::bind(socket_path(&path_a)).expect("socket a");
    let _listener_b = UnixListener::bind(socket_path(&path_b)).expect("socket b");
    for path in [&path_a, &path_b] {
        std::fs::write(
            metadata_path(path),
            serde_json::to_vec(&DaemonMetadata {
                version: DAEMON_METADATA_VERSION,
                pid: std::process::id(),
                project_root: None,
                session_id: "same-session".to_owned(),
                peer_entrypoint: false,
            })
            .expect("metadata json"),
        )
        .expect("write metadata");
    }

    assert!(matches!(
        find_harness_for_session("same-session"),
        Err(FindHarnessForSessionError::Ambiguous { .. })
    ));
}

/// Once two live claimants are proven, later truncation, cancellation, or
/// storage failure must retain the stronger true-ambiguity classification.
#[test]
fn incomplete_scan_preserves_proven_ambiguity() {
    let matches = vec![PathBuf::from("first"), PathBuf::from("second")];

    assert!(matches!(
        incomplete_or_ambiguous("same-session", &matches),
        FindHarnessForSessionError::Ambiguous {
            session_id,
            matches: classified,
        } if session_id == "same-session" && classified == matches
    ));
}

/// A lookup that exhausts the expanded raw-entry budget must not return a
/// live claimant because an omitted entry could advertise the same session.
#[test]
fn find_harness_for_session_fails_closed_when_scan_is_incomplete() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let dir = harnesses_dir();
    std::fs::create_dir_all(&dir).expect("harnesses dir");
    for index in 0..=SESSION_LOOKUP_MAX_DIRECTORY_ENTRIES {
        std::fs::write(dir.join(format!("junk-{index:04}")), b"x").expect("junk entry");
    }
    let claimant = dir.join("claimant");
    let _listener = UnixListener::bind(socket_path(&claimant)).expect("claimant socket");
    write_peer_metadata(&claimant, "bounded-session", temp.path(), true);

    assert!(matches!(
        find_harness_for_session("bounded-session"),
        Err(FindHarnessForSessionError::Incomplete { .. })
    ));
    assert!(socket_path(&claimant).exists());
    assert!(metadata_path(&claimant).exists());
}

/// A same-session record owned by a live process but temporarily
/// unreachable keeps uniqueness unproven even when another claimant
/// answers.
#[test]
fn find_harness_for_session_fails_closed_for_unreachable_live_claimant() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let dir = harnesses_dir();
    std::fs::create_dir_all(&dir).expect("harnesses dir");
    let reachable = dir.join("reachable");
    let _listener = UnixListener::bind(socket_path(&reachable)).expect("reachable socket");
    let unreachable = dir.join("unreachable");
    drop(UnixListener::bind(socket_path(&unreachable)).expect("unreachable socket"));
    for path in [&reachable, &unreachable] {
        std::fs::write(
            metadata_path(path),
            serde_json::to_vec(&DaemonMetadata {
                version: DAEMON_METADATA_VERSION,
                pid: std::process::id(),
                project_root: None,
                session_id: "same-session".to_owned(),
                peer_entrypoint: true,
            })
            .expect("metadata"),
        )
        .expect("write metadata");
    }

    assert!(matches!(
        find_harness_for_session("same-session"),
        Err(FindHarnessForSessionError::Incomplete { .. })
    ));
}

/// A partial metadata rewrite under a canonical live-PID instance stem may
/// hide a second claimant, so one valid live match cannot prove uniqueness.
#[test]
fn find_harness_for_session_fails_closed_for_malformed_live_metadata() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let dir = harnesses_dir();
    std::fs::create_dir_all(&dir).expect("harnesses dir");
    let reachable = dir.join("reachable");
    let _reachable_listener =
        UnixListener::bind(socket_path(&reachable)).expect("reachable socket");
    write_peer_metadata(&reachable, "same-session", temp.path(), true);
    let instance = HarnessInstanceId::parse("0123456789abcdef").expect("canonical instance id");
    let unresolved = harness_path_for_process(std::process::id(), &instance);
    let _unresolved_listener =
        UnixListener::bind(socket_path(&unresolved)).expect("unresolved socket");
    std::fs::write(metadata_path(&unresolved), b"{\"session_id\":").expect("partial metadata");

    assert!(matches!(
        find_harness_for_session("same-session"),
        Err(FindHarnessForSessionError::Incomplete { .. })
    ));
    assert!(socket_path(&unresolved).exists());
    assert!(metadata_path(&unresolved).exists());
}

/// A symlinked legacy numeric live-PID metadata record is never followed or
/// ignored when doing so could hide a second session claimant.
#[cfg(unix)]
#[test]
fn find_harness_for_session_fails_closed_for_symlinked_live_metadata() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let dir = harnesses_dir();
    std::fs::create_dir_all(&dir).expect("harnesses dir");
    let reachable = dir.join("reachable");
    let _reachable_listener =
        UnixListener::bind(socket_path(&reachable)).expect("reachable socket");
    write_peer_metadata(&reachable, "same-session", temp.path(), true);
    let unresolved = dir.join(std::process::id().to_string());
    let _unresolved_listener =
        UnixListener::bind(socket_path(&unresolved)).expect("unresolved socket");
    let target = temp.path().join("metadata-target");
    std::fs::write(&target, b"{}").expect("symlink target");
    std::os::unix::fs::symlink(&target, metadata_path(&unresolved)).expect("symlink metadata");

    assert!(matches!(
        find_harness_for_session("same-session"),
        Err(FindHarnessForSessionError::Incomplete { .. })
    ));
    assert!(socket_path(&unresolved).exists());
    assert!(metadata_path(&unresolved).is_symlink());
}

/// Oversized live-PID metadata remains unresolved without reading beyond
/// the byte bound or returning an early live match as unique.
#[test]
fn find_harness_for_session_fails_closed_for_oversized_live_metadata() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let dir = harnesses_dir();
    std::fs::create_dir_all(&dir).expect("harnesses dir");
    let reachable = dir.join("reachable");
    let _reachable_listener =
        UnixListener::bind(socket_path(&reachable)).expect("reachable socket");
    write_peer_metadata(&reachable, "same-session", temp.path(), true);
    let unresolved = dir.join(std::process::id().to_string());
    let _unresolved_listener =
        UnixListener::bind(socket_path(&unresolved)).expect("unresolved socket");
    std::fs::write(
        metadata_path(&unresolved),
        vec![b' '; SESSION_DISCOVERY_MAX_METADATA_BYTES as usize + 1],
    )
    .expect("oversized metadata");

    assert!(matches!(
        find_harness_for_session("same-session"),
        Err(FindHarnessForSessionError::Incomplete { .. })
    ));
    assert_eq!(
        std::fs::metadata(metadata_path(&unresolved))
            .expect("metadata remains")
            .len(),
        SESSION_DISCOVERY_MAX_METADATA_BYTES + 1
    );
}

/// Failure to enumerate an existing runtime catalog is incomplete rather
/// than evidence that no matching daemon exists.
#[test]
fn find_harness_for_session_fails_closed_when_catalog_is_not_a_directory() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    std::fs::create_dir_all(root_runtime_dir()).expect("runtime root");
    std::fs::write(harnesses_dir(), b"not a directory").expect("catalog marker");

    assert!(matches!(
        find_harness_for_session("missing-session"),
        Err(FindHarnessForSessionError::Incomplete { .. })
    ));
}

/// Ensures daemon startup creates private runtime directories rather than
/// relying on the process umask, protecting fallback IPC sockets and
/// metadata from other local users.
#[cfg(unix)]
#[test]
fn prepare_harness_paths_creates_private_runtime_dirs() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("project root");

    let paths = prepare_harness_paths(&project_root, "session").expect("paths");

    let root_mode = std::fs::metadata(root_runtime_dir())
        .expect("root metadata")
        .permissions()
        .mode()
        & 0o777;
    let harnesses_mode = std::fs::metadata(harnesses_dir())
        .expect("harnesses metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(root_mode, 0o700);
    assert_eq!(harnesses_mode, 0o700);
    assert_eq!(paths.path().parent(), Some(harnesses_dir().as_path()));
}

/// Ensures a pre-existing runtime symlink is refused instead of followed
/// before binding daemon IPC sockets or writing discovery metadata.
#[cfg(unix)]
#[test]
fn prepare_harness_paths_rejects_symlink_runtime_root() {
    let temp = TempDir::new().expect("temp runtime");
    let _guard = runtime_override(&temp);
    let real = temp.path().join("real");
    std::fs::create_dir_all(&real).expect("real runtime target");
    std::os::unix::fs::symlink(&real, root_runtime_dir()).expect("runtime symlink");
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("project root");

    let error = match prepare_harness_paths(&project_root, "session") {
        Ok(_) => panic!("symlink runtime root must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("must not be a symlink"));
}

fn write_metadata_with_pid(path: &Path, project_root: &Path, session_id: &str, pid: u32) {
    std::fs::write(
        metadata_path(path),
        serde_json::to_vec(&DaemonMetadata {
            version: DAEMON_METADATA_VERSION,
            pid,
            project_root: Some(project_root.to_path_buf()),
            session_id: session_id.to_owned(),
            peer_entrypoint: false,
        })
        .expect("metadata json"),
    )
    .expect("write metadata");
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn new() -> Self {
        Self {
            child: Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn sleep"),
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(target_os = "linux")]
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().expect("spawn true");
    let pid = child.id();
    child.wait().expect("wait true");
    pid
}
