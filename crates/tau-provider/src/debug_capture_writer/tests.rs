use std::io;
use std::sync::{Arc, Mutex, mpsc};

use tau_config::provider_debug_capture::ProviderDebugCaptureFilename;
use tempfile::TempDir;

use super::{CaptureJob, CaptureQueue, run_worker, write_capture, write_capture_with};

fn job(session_dir: &std::path::Path, filename: &str, json: &[u8]) -> CaptureJob {
    CaptureJob::new(
        session_dir.to_path_buf(),
        ProviderDebugCaptureFilename::parse(filename).expect("valid capture filename"),
        json.to_vec(),
    )
}

/// Ensures absolute, traversal, and malformed session spellings cannot reach
/// the typed shared capture API or its filesystem join.
#[test]
fn shared_capture_api_rejects_unsafe_session_identity() {
    for invalid in ["../escape", "/absolute", ".", "has/slash", "has space"] {
        assert!(
            tau_proto::SessionId::parse(invalid).is_err(),
            "{invalid} must not become a capture path"
        );
    }
}

/// Proves full queues reject new captures immediately rather than waiting for
/// worker progress.
#[test]
fn overload_drops_new_capture_without_blocking() {
    let (sender, _receiver) = mpsc::sync_channel(1);
    let queue = CaptureQueue { sender };
    queue
        .try_submit(job(
            std::path::Path::new("session"),
            "1-one-http-sse-request.json.zst",
            b"one",
        ))
        .expect("first job fills queue");

    let error = queue
        .try_submit(job(
            std::path::Path::new("session"),
            "2-two-http-sse-request.json.zst",
            b"two",
        ))
        .expect_err("full queue rejects next capture");

    assert!(matches!(error, mpsc::TrySendError::Full(_)));
}

/// Proves one write failure does not stop later accepted captures from running.
#[test]
fn write_failure_isolated_from_later_capture() {
    let (sender, receiver) = mpsc::sync_channel(2);
    sender
        .try_send(job(
            std::path::Path::new("session"),
            "1-one-http-sse-request.json.zst",
            b"one",
        ))
        .expect("first job");
    sender
        .try_send(job(
            std::path::Path::new("session"),
            "2-two-http-sse-request.json.zst",
            b"two",
        ))
        .expect("second job");
    drop(sender);
    let attempted = Arc::new(Mutex::new(Vec::new()));
    let worker_attempted = Arc::clone(&attempted);

    run_worker(receiver, move |job| {
        worker_attempted
            .lock()
            .expect("attempt list")
            .push(job.filename.as_str().to_owned());
        if job.filename.as_str().contains("-one-") {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
        } else {
            Ok(())
        }
    });

    assert_eq!(
        *attempted.lock().expect("attempt list"),
        [
            "1-one-http-sse-request.json.zst",
            "2-two-http-sse-request.json.zst"
        ]
    );
}

/// Proves the worker helper drains accepted captures when its test-only
/// producers disconnect; production intentionally keeps its sender for process
/// lifetime and does not use this as a shutdown guarantee.
#[test]
fn worker_drains_when_test_producers_disconnect() {
    let (sender, receiver) = mpsc::sync_channel(2);
    sender
        .try_send(job(
            std::path::Path::new("session"),
            "1-one-http-sse-request.json.zst",
            b"one",
        ))
        .expect("first job");
    sender
        .try_send(job(
            std::path::Path::new("session"),
            "2-two-http-sse-request.json.zst",
            b"two",
        ))
        .expect("second job");
    drop(sender);
    let mut attempted = Vec::new();

    run_worker(receiver, |job| {
        attempted.push(job.filename.as_str().to_owned());
        Ok(())
    });

    assert_eq!(
        attempted,
        [
            "1-one-http-sse-request.json.zst",
            "2-two-http-sse-request.json.zst"
        ]
    );
}

/// Proves production captures are zstd streams containing the exact serialized
/// JSON and that missing session roots are never created.
#[test]
fn production_writer_compresses_json_and_requires_existing_session() {
    let temp = TempDir::new().expect("temp state");
    let session = temp.path().join("session");
    std::fs::create_dir(&session).expect("session");
    let json = br#"{"secret":"debug"}"#;
    let capture = job(&session, "1-prompt-http-sse-request.json.zst", json);

    write_capture(&capture).expect("write capture");

    let path = session.join("debug/provider-requests/1-prompt-http-sse-request.json.zst");
    let decoded =
        zstd::stream::decode_all(std::fs::File::open(path).expect("capture")).expect("decode zstd");
    assert_eq!(decoded, json);

    let missing = temp.path().join("missing");
    assert!(write_capture(&job(&missing, "2-prompt-http-sse-request.json.zst", json)).is_err());
    assert!(!missing.exists());
}

/// Proves the worker refuses symlinked debug descendants rather than writing
/// sensitive captures outside the durable session directory.
#[cfg(unix)]
#[test]
fn production_writer_rejects_symlinked_debug_directory() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp state");
    let session = temp.path().join("session");
    let external = temp.path().join("external");
    std::fs::create_dir(&session).expect("session");
    std::fs::create_dir(&external).expect("external");
    symlink(&external, session.join("debug")).expect("debug symlink");

    let result = write_capture(&job(
        &session,
        "1-prompt-http-sse-request.json.zst",
        br#"{"secret":"debug"}"#,
    ));

    assert!(result.is_err());
    assert!(
        std::fs::read_dir(&external)
            .expect("external")
            .next()
            .is_none()
    );
}

/// Proves the worker refuses both a symlinked session root and a symlinked
/// existing provider-capture directory.
#[cfg(unix)]
#[test]
fn production_writer_rejects_symlinked_session_and_capture_directories() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp state");
    let external = temp.path().join("external");
    std::fs::create_dir(&external).expect("external");
    let linked_session = temp.path().join("linked-session");
    symlink(&external, &linked_session).expect("session symlink");
    assert!(
        write_capture(&job(
            &linked_session,
            "1-prompt-http-sse-request.json.zst",
            b"capture",
        ))
        .is_err()
    );

    let session = temp.path().join("session");
    let debug = session.join("debug");
    std::fs::create_dir_all(&debug).expect("debug");
    symlink(&external, debug.join("provider-requests")).expect("capture symlink");
    assert!(
        write_capture(&job(
            &session,
            "2-prompt-websocket-response.json.zst",
            b"capture",
        ))
        .is_err()
    );
    assert!(
        std::fs::read_dir(&external)
            .expect("external")
            .next()
            .is_none()
    );
}

/// Defines write-failure semantics: a streaming failure can leave a truncated
/// final `.json.zst` artifact, but the error remains local to the worker job.
#[test]
fn streaming_write_failure_can_leave_truncated_final_artifact() {
    use std::io::Write as _;

    let temp = TempDir::new().expect("temp state");
    let session = temp.path().join("session");
    std::fs::create_dir(&session).expect("session");
    let capture = job(&session, "1-prompt-http-sse-request.json.zst", b"complete");

    let result = write_capture_with(&capture, |mut file, _json| {
        file.write_all(b"truncated")?;
        Err(io::Error::new(io::ErrorKind::WriteZero, "injected"))
    });

    assert!(result.is_err());
    assert_eq!(
        std::fs::read(session.join("debug/provider-requests/1-prompt-http-sse-request.json.zst"))
            .expect("truncated artifact"),
        b"truncated"
    );
}
