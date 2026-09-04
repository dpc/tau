use std::fs::Permissions;
use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixListener;
use std::sync::Barrier;
use std::thread::JoinHandle;

use tempfile::TempDir;

use super::*;

fn session(value: &str) -> tau_proto::SessionId {
    tau_proto::SessionId::parse(value).expect("valid test session")
}

fn bounded_runtime_root() -> TempDir {
    const SOCKET_SUFFIX_BYTES: usize = 1 + 3 + 1 + 9 + 1 + 7 + 1 + 64 + 5;
    let configured = tempfile::env::temp_dir();
    for parent in [
        Path::new("/tmp"),
        Path::new("/dev/shm"),
        configured.parent().unwrap_or(configured.as_path()),
    ] {
        if let Ok(root) = tempfile::Builder::new().prefix("t").tempdir_in(parent)
            && root.path().as_os_str().as_encoded_bytes().len() + SOCKET_SUFFIX_BYTES <= 107
        {
            return root;
        }
    }
    panic!("no bounded runtime root");
}

fn spawn_peer_daemon(
    root: &TempDir,
    session_id: &str,
    available: bool,
) -> (SessionClaim, JoinHandle<()>) {
    let id = session(session_id);
    let mut claim = claim_session(root.path(), &id).expect("claim peer session");
    claim.reclaim_stale_socket().expect("reclaim peer socket");
    let listener = UnixListener::bind(claim.socket_path()).expect("bind peer socket");
    claim.publish(true).expect("publish peer claim");
    let thread = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept peer probe");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("peer read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .expect("peer write timeout");
        let reader_stream = stream.try_clone().expect("clone peer stream");
        let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(reader_stream));
        let mut writer = tau_proto::HarnessOutputWriter::new(BufWriter::new(stream));
        match reader.read_message().expect("read peer hello") {
            Some(tau_proto::HarnessInputMessage::Hello(hello))
                if hello.expected_session_id.as_ref() == Some(&id) => {}
            other => panic!("expected exact peer hello, got {other:?}"),
        }
        writer
            .write_message(&tau_proto::HarnessOutputMessage::SessionAccepted(
                tau_proto::SessionAccepted {
                    session_id: id.clone(),
                },
            ))
            .expect("write peer acceptance");
        writer.flush().expect("flush peer acceptance");
        let request = match reader.read_message().expect("read peer request") {
            Some(tau_proto::HarnessInputMessage::PeerSessionProbe(request)) => request,
            other => panic!("expected peer probe, got {other:?}"),
        };
        writer
            .write_message(&tau_proto::HarnessOutputMessage::PeerSessionProbeResult(
                tau_proto::PeerSessionProbeResult {
                    request_id: request.request_id,
                    available,
                },
            ))
            .expect("write peer result");
        writer.flush().expect("flush peer result");
    });
    (claim, thread)
}

#[derive(Clone, Copy)]
enum ExactProbeReply {
    Correlated,
    Close,
    Timeout,
}

fn spawn_exact_probe_daemon(
    root: &TempDir,
    session_id: &str,
    reply: ExactProbeReply,
) -> (SessionClaim, JoinHandle<()>) {
    let id = session(session_id);
    let mut claim = claim_session(root.path(), &id).expect("claim exact session");
    claim.reclaim_stale_socket().expect("reclaim exact socket");
    let listener = UnixListener::bind(claim.socket_path()).expect("bind exact socket");
    claim.publish(false).expect("publish exact claim");
    let wrong_project = root.path().join("wrong");
    let project = root.path().join("project");
    let thread = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept exact probe");
        if matches!(reply, ExactProbeReply::Close) {
            return;
        }
        if matches!(reply, ExactProbeReply::Timeout) {
            std::thread::sleep(PROBE_TIMEOUT + Duration::from_millis(50));
            return;
        }
        let reader_stream = stream.try_clone().expect("clone exact stream");
        let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(reader_stream));
        let mut writer = tau_proto::HarnessOutputWriter::new(BufWriter::new(stream));
        match reader.read_message().expect("read exact hello") {
            Some(tau_proto::HarnessInputMessage::Hello(hello))
                if hello.expected_session_id.as_ref() == Some(&id) => {}
            other => panic!("expected exact hello, got {other:?}"),
        }
        writer
            .write_message(&tau_proto::HarnessOutputMessage::SessionAccepted(
                tau_proto::SessionAccepted {
                    session_id: id.clone(),
                },
            ))
            .expect("write exact acceptance");
        writer.flush().expect("flush exact acceptance");
        let request = match reader.read_message().expect("read exact request") {
            Some(tau_proto::HarnessInputMessage::GetCurrentSession(request)) => request,
            other => panic!("expected current-session request, got {other:?}"),
        };
        writer
            .write_message(&tau_proto::HarnessOutputMessage::CurrentSessionResult(
                tau_proto::CurrentSessionResult {
                    request_id: "unrelated".to_owned(),
                    session_id: id.clone(),
                    project_root: wrong_project,
                },
            ))
            .expect("write unrelated result");
        writer
            .write_message(&tau_proto::HarnessOutputMessage::CurrentSessionResult(
                tau_proto::CurrentSessionResult {
                    request_id: request.request_id,
                    session_id: id,
                    project_root: project,
                },
            ))
            .expect("write correlated result");
        writer.flush().expect("flush exact results");
    });
    (claim, thread)
}

/// Session ids, rather than process ids or runtime generations, must produce
/// stable bounded claim and socket paths.
#[test]
fn session_keyed_paths_are_deterministic_and_short() {
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session(&"s".repeat(tau_proto::SESSION_SCOPED_ID_MAX_LEN));
    let first = harness_path_for_session(&id);
    assert_eq!(first, harness_path_for_session(&id));
    assert_eq!(
        first
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::len),
        Some(64)
    );
    assert!(
        !first
            .to_string_lossy()
            .contains(&std::process::id().to_string())
    );
}

/// An absent claim and an unlocked stale claim both linearize as not running;
/// ordinary resolution must never create or delete runtime paths.
#[test]
fn absent_and_unlocked_claims_are_not_running() {
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session("session-a");
    assert_eq!(
        find_harness_for_session(id.as_str()).expect("absent lookup"),
        None
    );
    let claim = claim_session(root.path(), &id).expect("claim session");
    let path = claim.claim_path.clone();
    drop(claim);
    std::fs::write(&path, b"stale").expect("stale residue");
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).expect("claim mode");
    assert_eq!(
        find_harness_for_session(id.as_str()).expect("unlocked lookup"),
        None
    );
    assert!(path.exists(), "resolver must not clean unlocked residue");
}

/// Exactly one concurrent daemon may own a session claim, and a losing starter
/// must not disturb the winner's deterministic socket path.
#[test]
fn concurrent_claim_loser_cannot_reclaim_winner() {
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session("session-race");
    let winner = claim_session(root.path(), &id).expect("winner claim");
    let error = claim_session(root.path(), &id)
        .err()
        .expect("loser must fail");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(
        winner.socket_path(),
        socket_path(&harness_path_for_session(&id))
    );
}

/// A contended claim without a ready exact responder is incomplete rather than
/// absent, preventing a caller from routing around startup.
#[test]
fn contended_unreachable_claim_is_incomplete() {
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session("session-starting");
    let mut claim = claim_session(root.path(), &id).expect("claim session");
    claim.publish(false).expect("publish diagnostics");
    assert!(matches!(
        find_harness_for_session(id.as_str()),
        Err(FindHarnessForSessionError::Incomplete { session_id }) if session_id == id.as_str()
    ));
}

/// Only the lock winner may reclaim a same-owner stale socket, while a
/// non-socket path at the deterministic address fails closed.
#[test]
fn claim_owner_reclaims_only_stale_socket() {
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session("session-stale");
    let claim = claim_session(root.path(), &id).expect("claim session");
    let stale = UnixListener::bind(claim.socket_path()).expect("create stale socket");
    drop(stale);
    claim.reclaim_stale_socket().expect("reclaim stale socket");
    assert!(!claim.socket_path().exists());
    std::fs::write(claim.socket_path(), b"not a socket").expect("blocking file");
    let error = claim
        .reclaim_stale_socket()
        .expect_err("non-socket must fail");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

/// Pathname replacement cannot trick claim retirement into deleting a file the
/// daemon never locked.
#[test]
fn claim_retirement_preserves_replacement_inode() {
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session("session-replaced-claim");
    let claim = claim_session(root.path(), &id).expect("claim session");
    let path = claim.claim_path.clone();
    std::fs::remove_file(&path).expect("unlink locked claim pathname");
    std::fs::write(&path, b"replacement").expect("create replacement claim");
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).expect("replacement mode");

    drop(claim);

    assert_eq!(
        std::fs::read(path).expect("replacement survives"),
        b"replacement"
    );
}

/// A contended record whose identity does not match its deterministic key is
/// incomplete and never routes to the key owner's socket.
#[test]
fn mismatched_contended_record_fails_closed() {
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let requested = session("session-requested");
    let other = session("session-other");
    let mut claim = claim_session(root.path(), &requested).expect("claim session");
    claim.record.session_id = other;
    claim
        .publish(false)
        .expect("publish mismatched diagnostics");

    assert!(matches!(
        find_harness_for_session(requested.as_str()),
        Err(FindHarnessForSessionError::Incomplete { session_id })
            if session_id == requested.as_str()
    ));
}

/// Orderly retirement removes exactly the lock pathname owned by the claim.
#[test]
fn orderly_retirement_removes_owned_claim() {
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session("session-retired");
    let claim = claim_session(root.path(), &id).expect("claim session");
    let path = claim.claim_path.clone();

    claim.retire().expect("retire claim");

    assert!(!path.exists());
    assert_eq!(
        find_harness_for_session(id.as_str()).expect("retired lookup"),
        None
    );
}

/// Both local listing and peer discovery bound raw directory traversal, not
/// only the number of valid live records they happen to return.
#[test]
fn ignored_entry_flood_marks_local_and_peer_listing_incomplete() {
    let _serial = TEST_DISCOVERY_SERIAL.lock().expect("discovery serial lock");
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session("entry-flood-bootstrap");
    drop(claim_session(root.path(), &id).expect("prepare claim directory"));
    let claims = claims_dir();
    for index in 0..=MAX_DIRECTORY_ENTRIES {
        std::fs::write(claims.join(format!("ignored-{index}")), b"").expect("ignored entry");
    }

    let error = list_running_sessions().expect_err("local listing must reject exhaustion");
    assert!(
        error
            .to_string()
            .contains("could not list every running session claim")
    );
    assert!(
        list_running_claim_records().is_err(),
        "peer listing must reject the same raw-entry exhaustion"
    );
}

/// Whole-call admission is non-queued and restores every slot after release.
#[test]
fn peer_discovery_whole_call_admission_is_non_queued() {
    let _serial = TEST_DISCOVERY_SERIAL.lock().expect("discovery serial lock");
    let permits = (0..MAX_DISCOVERY_CALLS)
        .map(|_| DiscoveryCallPermit::try_acquire().expect("discovery permit"))
        .collect::<Vec<_>>();
    assert!(DiscoveryCallPermit::try_acquire().is_none());
    drop(permits);
    assert!(DiscoveryCallPermit::try_acquire().is_some());
}

/// A blocked claim-directory scan returns at the one whole-call deadline while
/// its permit remains charged until the isolated worker exits.
#[test]
fn peer_discovery_slow_storage_isolated_by_total_deadline() {
    let _serial = TEST_DISCOVERY_SERIAL.lock().expect("discovery serial lock");
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session("slow-scan-bootstrap");
    drop(claim_session(root.path(), &id).expect("prepare claim directory"));
    *TEST_DISCOVERY_SCAN_DELAY
        .lock()
        .expect("scan delay lock poisoned") = Some((claims_dir(), 2_100));
    let started = Instant::now();

    let snapshot = discover_peer_sessions(
        None,
        SESSION_DISCOVERY_MAX_RESULTS,
        "",
        DiscoveryCallPermit::try_acquire().expect("discovery permit"),
    );

    *TEST_DISCOVERY_SCAN_DELAY
        .lock()
        .expect("scan delay lock poisoned") = None;
    assert!(started.elapsed() < DISCOVERY_TIMEOUT + Duration::from_secs(1));
    assert!(snapshot.sessions.is_empty());
    assert!(snapshot.scan_truncated);
    let worker_deadline = Instant::now() + Duration::from_secs(1);
    while ACTIVE_DISCOVERY_WORKERS.load(Ordering::Acquire) != 0 {
        assert!(
            Instant::now() < worker_deadline,
            "isolated discovery worker did not retire"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Result-limit truncation remains explicit after exact admission and a
/// positive peer-entrypoint response.
#[test]
fn peer_discovery_reports_result_truncation() {
    let _serial = TEST_DISCOVERY_SERIAL.lock().expect("discovery serial lock");
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let (claim, daemon) = spawn_peer_daemon(&root, "peer-result-limit", true);

    let snapshot = discover_peer_sessions(
        None,
        0,
        "",
        DiscoveryCallPermit::try_acquire().expect("discovery permit"),
    );

    assert!(snapshot.sessions.is_empty());
    assert!(snapshot.truncated);
    assert!(!snapshot.scan_truncated);
    daemon.join().expect("peer daemon");
    drop(claim);
}

/// A failed opted-in probe makes the whole snapshot incomplete rather than
/// returning a partial set of successfully probed peers.
#[test]
fn peer_discovery_rejects_partial_results_after_probe_failure() {
    let _serial = TEST_DISCOVERY_SERIAL.lock().expect("discovery serial lock");
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let (live_claim, live_daemon) = spawn_peer_daemon(&root, "peer-live", true);
    let blocked_id = session("peer-blocked");
    let mut blocked_claim =
        claim_session(root.path(), &blocked_id).expect("claim blocked peer session");
    blocked_claim
        .reclaim_stale_socket()
        .expect("reclaim blocked socket");
    let blocked_listener =
        UnixListener::bind(blocked_claim.socket_path()).expect("bind blocked peer socket");
    blocked_claim
        .publish(true)
        .expect("publish blocked peer claim");

    let snapshot = discover_peer_sessions(
        None,
        SESSION_DISCOVERY_MAX_RESULTS,
        "",
        DiscoveryCallPermit::try_acquire().expect("discovery permit"),
    );

    assert!(snapshot.sessions.is_empty());
    assert!(snapshot.scan_truncated);
    live_daemon.join().expect("live peer daemon");
    drop(live_claim);
    drop(blocked_listener);
    drop(blocked_claim);
}

/// Saturated global probe slots keep the candidate queued through the total
/// deadline; cancellation prevents a socket connect after return.
#[test]
fn peer_discovery_total_deadline_cancels_queued_probe_before_io() {
    let _serial = TEST_DISCOVERY_SERIAL.lock().expect("discovery serial lock");
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session("peer-queued");
    let mut claim = claim_session(root.path(), &id).expect("claim queued peer");
    claim.reclaim_stale_socket().expect("reclaim queued socket");
    let listener = UnixListener::bind(claim.socket_path()).expect("bind queued socket");
    listener
        .set_nonblocking(true)
        .expect("set queued listener nonblocking");
    claim.publish(true).expect("publish queued peer");
    let cancelled = AtomicBool::new(false);
    let slots = (0..MAX_DISCOVERY_PROBES)
        .map(|_| {
            DiscoveryProbeSlot::acquire(Instant::now() + Duration::from_secs(3), &cancelled)
                .expect("probe slot")
        })
        .collect::<Vec<_>>();

    let snapshot = discover_peer_sessions(
        None,
        SESSION_DISCOVERY_MAX_RESULTS,
        "",
        DiscoveryCallPermit::try_acquire().expect("discovery permit"),
    );

    assert!(snapshot.sessions.is_empty());
    assert!(snapshot.scan_truncated);
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
    ));
    assert_eq!(ACTIVE_DISCOVERY_WORKERS.load(Ordering::Acquire), 0);
    drop(slots);
}

/// Exact resolution rechecks cancellation after reading the authoritative
/// claim and before performing any socket I/O.
#[test]
fn find_harness_for_session_checks_cancellation_after_claim_read() {
    let _serial = TEST_DISCOVERY_SERIAL.lock().expect("discovery serial lock");
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session("cancel-after-claim");
    let mut claim = claim_session(root.path(), &id).expect("claim session");
    claim.reclaim_stale_socket().expect("reclaim socket");
    let listener = UnixListener::bind(claim.socket_path()).expect("bind socket");
    listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");
    claim.publish(false).expect("publish claim");
    TEST_CANCEL_AFTER_CLAIM_READ.store(true, Ordering::Release);
    let cancelled = AtomicBool::new(false);

    assert!(matches!(
        find_harness_for_session_until(
            id.as_str(),
            Instant::now() + Duration::from_secs(1),
            &cancelled,
        ),
        Err(FindHarnessForSessionError::Incomplete { .. })
    ));
    assert!(cancelled.load(Ordering::Acquire));
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
    ));
}

/// Exact discovery ignores unrelated current-session responses and accepts only
/// the correlated response for the admitted immutable session.
#[test]
fn exact_session_probe_correlates_authoritative_response() {
    let _serial = TEST_DISCOVERY_SERIAL.lock().expect("discovery serial lock");
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session("exact-correlated");
    let (claim, daemon) = spawn_exact_probe_daemon(&root, id.as_str(), ExactProbeReply::Correlated);
    let expected_project = root.path().join("project");

    assert_eq!(
        probe_exact_session(
            &harness_path_for_session(&id),
            &id,
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(false),
        ),
        Some(RunningSession {
            session_id: id,
            project_root: expected_project,
        })
    );
    daemon.join().expect("exact daemon");
    drop(claim);
}

/// Timeout and closure before a complete correlated response both classify the
/// exact claimed daemon as unresponsive.
#[test]
fn exact_session_probe_maps_timeout_and_closure_to_unresponsive() {
    let _serial = TEST_DISCOVERY_SERIAL.lock().expect("discovery serial lock");
    for (suffix, reply) in [
        ("close", ExactProbeReply::Close),
        ("timeout", ExactProbeReply::Timeout),
    ] {
        let root = bounded_runtime_root();
        let _override = override_runtime_dir(root.path());
        let id = session(&format!("exact-{suffix}"));
        let (claim, daemon) = spawn_exact_probe_daemon(&root, id.as_str(), reply);

        assert_eq!(
            probe_exact_session(
                &harness_path_for_session(&id),
                &id,
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(false),
            ),
            None
        );
        daemon.join().expect("exact daemon");
        drop(claim);
    }
}

/// Every blocking probe stage derives its remaining allowance from one absolute
/// deadline rather than restarting the per-probe cap.
#[test]
fn probe_stage_budget_observes_one_absolute_deadline() {
    let cancelled = AtomicBool::new(false);
    let started = Instant::now();
    let deadline = started + PROBE_TIMEOUT;

    assert_eq!(
        probe_remaining_at(deadline, &cancelled, started),
        Some(PROBE_TIMEOUT)
    );
    assert_eq!(
        probe_remaining_at(
            deadline,
            &cancelled,
            started + PROBE_TIMEOUT - Duration::from_millis(10),
        ),
        Some(Duration::from_millis(10))
    );
    assert_eq!(
        probe_remaining_at(deadline, &cancelled, deadline),
        Some(Duration::ZERO)
    );
    assert_eq!(
        probe_remaining_at(deadline, &cancelled, deadline + Duration::from_nanos(1)),
        None
    );
    cancelled.store(true, Ordering::Release);
    assert_eq!(probe_remaining_at(deadline, &cancelled, started), None);
}

/// Public running-session listing isolates blocking claim traversal behind the
/// same whole-call deadline as peer discovery.
#[test]
fn running_session_list_isolates_slow_storage() {
    let _serial = TEST_DISCOVERY_SERIAL.lock().expect("discovery serial lock");
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session("slow-list-bootstrap");
    drop(claim_session(root.path(), &id).expect("prepare claim directory"));
    *TEST_DISCOVERY_SCAN_DELAY
        .lock()
        .expect("scan delay lock poisoned") = Some((claims_dir(), 2_100));
    let started = Instant::now();

    let result = list_running_sessions();

    *TEST_DISCOVERY_SCAN_DELAY
        .lock()
        .expect("scan delay lock poisoned") = None;
    assert!(result.is_err());
    assert!(started.elapsed() < DISCOVERY_TIMEOUT + Duration::from_secs(1));
    let worker_deadline = Instant::now() + Duration::from_secs(1);
    while ACTIVE_DISCOVERY_WORKERS.load(Ordering::Acquire) != 0 {
        assert!(
            Instant::now() < worker_deadline,
            "isolated listing worker did not retire"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Public listing returns only reachable claims and sorts their exact session
/// identities; an empty claim directory remains a successful empty result.
#[test]
fn running_session_list_is_reachable_sorted_and_empty_when_absent() {
    let _serial = TEST_DISCOVERY_SERIAL.lock().expect("discovery serial lock");
    let empty = bounded_runtime_root();
    {
        let _override = override_runtime_dir(empty.path());
        assert!(list_running_sessions().expect("empty listing").is_empty());
    }

    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let (claim_b, daemon_b) =
        spawn_exact_probe_daemon(&root, "session-b", ExactProbeReply::Correlated);
    let (claim_a, daemon_a) =
        spawn_exact_probe_daemon(&root, "session-a", ExactProbeReply::Correlated);

    let sessions = list_running_sessions().expect("reachable listing");

    assert_eq!(
        sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["session-a", "session-b"]
    );
    daemon_a.join().expect("session-a daemon");
    daemon_b.join().expect("session-b daemon");
    drop((claim_a, claim_b));
}

/// Peer discovery ignores claims that did not opt in and exposes only the
/// project basename for an admitted peer.
#[test]
fn peer_discovery_requires_opt_in_and_redacts_project_path() {
    let _serial = TEST_DISCOVERY_SERIAL.lock().expect("discovery serial lock");
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let (live_claim, live_daemon) = spawn_peer_daemon(&root, "peer-redacted", true);
    let quiet_id = session("peer-not-opted-in");
    let mut quiet_claim = claim_session(root.path(), &quiet_id).expect("claim quiet session");
    quiet_claim
        .reclaim_stale_socket()
        .expect("reclaim quiet socket");
    let quiet_listener = UnixListener::bind(quiet_claim.socket_path()).expect("bind quiet socket");
    quiet_listener
        .set_nonblocking(true)
        .expect("quiet nonblocking");
    quiet_claim.publish(false).expect("publish quiet claim");

    let snapshot = discover_peer_sessions(
        None,
        SESSION_DISCOVERY_MAX_RESULTS,
        "",
        DiscoveryCallPermit::try_acquire().expect("discovery permit"),
    );

    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].session_id, "peer-redacted");
    assert_eq!(
        snapshot.sessions[0].project_label.as_deref(),
        root.path().file_name().and_then(|name| name.to_str())
    );
    assert!(!format!("{:?}", snapshot.sessions).contains(&root.path().display().to_string()));
    assert!(matches!(
        quiet_listener.accept(),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
    ));
    live_daemon.join().expect("live daemon");
    drop((live_claim, quiet_claim));
}

/// Claim decoding accepts the exact byte limit and rejects one byte more.
#[test]
fn claim_record_byte_limit_is_inclusive_and_bounded() {
    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let id = session("claim-byte-boundary");
    let mut claim = claim_session(root.path(), &id).expect("claim session");
    let base = serde_json::to_vec(&claim.record).expect("encode claim");
    let mut exact = base;
    exact.resize(MAX_CLAIM_BYTES as usize, b' ');
    claim.file.set_len(0).expect("truncate claim");
    claim.file.write_all(&exact).expect("write exact claim");
    claim.file.flush().expect("flush exact claim");
    assert_eq!(
        read_claim(&mut claim.file)
            .expect("read exact claim")
            .session_id,
        id
    );

    claim.file.set_len(0).expect("truncate oversized claim");
    exact.push(b' ');
    claim.file.write_all(&exact).expect("write oversized claim");
    claim.file.flush().expect("flush oversized claim");
    assert!(read_claim(&mut claim.file).is_err());
}

/// Thread-scoped runtime overrides remain independent across overlapping
/// embedded operations.
#[test]
fn scoped_runtime_directories_do_not_cross_concurrent_threads() {
    let first = bounded_runtime_root();
    let second = bounded_runtime_root();
    let first_path = first.path().to_path_buf();
    let second_path = second.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));
    let threads = [first_path.clone(), second_path.clone()].map(|path| {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            with_runtime_dir(Some(&path), || {
                barrier.wait();
                assert_eq!(root_runtime_dir(), path.join("tau"));
                prepare_harnesses_dir().expect("prepare scoped runtime");
                assert!(claims_dir().starts_with(&path));
            });
        })
    });
    for thread in threads {
        thread.join().expect("runtime scope");
    }
    assert_ne!(first_path, second_path);
}

/// Runtime coordination directories are private and a symlink at the authority
/// boundary is rejected rather than followed.
#[test]
fn runtime_directories_are_private_and_reject_symlink_authority() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let root = bounded_runtime_root();
    let _override = override_runtime_dir(root.path());
    let harnesses = prepare_harnesses_dir().expect("prepare runtime");
    for path in [harnesses.clone(), claims_dir(), sockets_dir()] {
        assert_eq!(
            std::fs::metadata(path)
                .expect("runtime metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    std::fs::remove_dir_all(&harnesses).expect("remove harness directory");
    let target = root.path().join("symlink-target");
    std::fs::create_dir(&target).expect("create symlink target");
    symlink(&target, &harnesses).expect("install harness symlink");
    assert_eq!(
        prepare_harnesses_dir()
            .expect_err("symlink authority must fail")
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}
