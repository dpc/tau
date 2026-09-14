use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;

use tau_cli_term_raw::{CursorShape, Event, Term};
use tau_harness::runtime_dir::RunningSession;

use super::*;

/// Creates a validated session identifier for focused completion tests.
fn session_id(value: &str) -> SessionId {
    SessionId::parse(value).expect("test session id")
}

/// A deliberately blocked cold discovery must not block completion reads,
/// and repeated demand while it runs must remain one discovery call.
#[test]
fn cold_discovery_is_nonblocking_and_single_flight() {
    let (term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(Vec::<u8>::new()), CursorShape::Bar);
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let calls = Arc::new(AtomicUsize::new(0));
    let discovery_calls = Arc::clone(&calls);
    let discovery = Arc::new(move || {
        discovery_calls.fetch_add(1, Ordering::AcqRel);
        started_tx.send(()).expect("discovery started");
        release_rx
            .lock()
            .expect("release receiver")
            .recv()
            .expect("release discovery");
        Ok(RunningSessionSnapshot {
            sessions: vec![RunningSession {
                session_id: session_id("session-one"),
                project_root: PathBuf::from("/work/one"),
            }],
            incomplete_claims: 0,
        })
    });
    let completion = SessionCompletion::new_with(
        session_id("current"),
        handle,
        Arc::new(Instant::now),
        discovery,
    );
    let completer = completion.completer();

    assert!(completer(&[""]).is_empty());
    started_rx.recv().expect("worker entered discovery");
    assert!(completer(&["sess"]).is_empty());
    assert!(completer(&["one"]).is_empty());
    assert_eq!(calls.load(Ordering::Acquire), 1);

    release_tx.send(()).expect("release worker");
    assert!(matches!(
        term.get_next_event().expect("completion refresh"),
        Event::CompletionRefresh
    ));
    let items = completer(&["ONE"]);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].value, "session-one");
}

/// Cache policy keeps a last-good snapshot only for the bounded stale
/// window and applies the short retry backoff after top-level failures.
#[test]
fn cache_applies_freshness_backoff_and_stale_cutoff() {
    let start = Instant::now();
    let mut cache = Cache::default();
    cache.record_success(
        start,
        vec![IndexedCompletion {
            item: CompletionItem::new("session-one", "/work/one"),
            lower_value: "session-one".to_owned(),
        }],
    );

    let (fresh, refresh) = cache.read(start + FRESH_TTL);
    assert_eq!(fresh.len(), 1);
    assert!(!refresh);
    let (stale, refresh) = cache.read(start + FRESH_TTL + Duration::from_millis(1));
    assert_eq!(stale.len(), 1);
    assert!(refresh);

    cache.record_failure(start + Duration::from_secs(6));
    let (backed_off, refresh) = cache.read(start + Duration::from_secs(7));
    assert_eq!(backed_off.len(), 1);
    assert!(!refresh);
    let (_, refresh) = cache.read(start + Duration::from_secs(8));
    assert!(refresh);
    let (expired, refresh) = cache.read(start + STALE_TTL + Duration::from_millis(1));
    assert!(expired.is_empty());
    assert!(refresh);
}

/// Successful snapshots replace the whole candidate set and annotate only
/// the exact current session while preserving project paths as
/// descriptions.
#[test]
fn snapshot_conversion_replaces_rows_and_marks_current_session() {
    let items = completion_items(
        RunningSessionSnapshot {
            sessions: vec![
                RunningSession {
                    session_id: session_id("current"),
                    project_root: PathBuf::from("/work/current"),
                },
                RunningSession {
                    session_id: session_id("other"),
                    project_root: PathBuf::from("/work/other"),
                },
            ],
            incomplete_claims: 2,
        },
        &session_id("current"),
    );

    assert_eq!(items[0].item.value, "current");
    assert_eq!(items[0].item.description, "/work/current (current)");
    assert_eq!(items[1].item.value, "other");
    assert_eq!(items[1].item.description, "/work/other");
}

/// Attachment shutdown during an in-flight scan joins after that bounded
/// scan returns instead of losing its wakeup and blocking forever.
#[test]
fn shutdown_during_discovery_exits_worker_after_result() {
    let (_term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(Vec::<u8>::new()), CursorShape::Bar);
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let completion = SessionCompletion::new_with(
        session_id("current"),
        handle,
        Arc::new(Instant::now),
        Arc::new(move || {
            started_tx.send(()).expect("discovery started");
            release_rx
                .lock()
                .expect("release receiver")
                .recv()
                .expect("release discovery");
            Ok(RunningSessionSnapshot {
                sessions: Vec::new(),
                incomplete_claims: 0,
            })
        }),
    );
    assert!(completion.completer()(&[""]).is_empty());
    started_rx.recv().expect("worker entered discovery");
    let (dropped_tx, dropped_rx) = mpsc::channel();
    std::thread::spawn(move || {
        drop(completion);
        dropped_tx.send(()).expect("report dropped");
    });

    release_tx.send(()).expect("release discovery");
    dropped_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("worker joined after discovery");
}
