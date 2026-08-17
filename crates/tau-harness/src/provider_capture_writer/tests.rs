use std::sync::mpsc;

use tempfile::TempDir;

use super::*;

/// Build one deterministic harness-owned write job.
fn job(session_dir: &Path, bytes: &[u8]) -> CaptureWriteJob {
    let prompt = tau_proto::AgentPromptId::parse("prompt").expect("prompt");
    CaptureWriteJob {
        session_dir: session_dir.to_path_buf(),
        provider_instance: tau_proto::ExtensionName::parse("provider").expect("provider"),
        filename: ProviderDebugCaptureFilename::new(
            1,
            &prompt,
            tau_proto::ProviderDebugCaptureClass::HttpSseRequest,
        ),
        zstd: bytes.to_vec(),
    }
}

/// Proves the harness writes opaque bytes under its attributed instance path
/// without parsing or decompressing them.
#[test]
fn writes_opaque_bytes_to_harness_owned_path() {
    let temp = TempDir::new().expect("temp");
    let session = temp.path().join("session");
    fs::create_dir(&session).expect("session");
    let bytes = b"not inspected as zstd";
    write_capture(&job(&session, bytes)).expect("write");
    assert_eq!(
        fs::read(
            session.join("debug/provider-requests/provider/1-prompt-http-sse-request.json.zst")
        )
        .expect("capture"),
        bytes
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let capture_root = session.join("debug/provider-requests");
        let instance = capture_root.join("provider");
        let capture = instance.join("1-prompt-http-sse-request.json.zst");
        for directory in [session.join("debug"), capture_root, instance] {
            assert_eq!(
                fs::metadata(directory)
                    .expect("capture directory")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        assert_eq!(
            fs::metadata(capture)
                .expect("capture file")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

/// Existing capture directories with permissive modes must be tightened before
/// a private provider artifact is created.
#[cfg(unix)]
#[test]
fn tightens_existing_capture_directory_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = TempDir::new().expect("temp");
    let session = temp.path().join("session");
    let debug = session.join("debug");
    let captures = debug.join("provider-requests");
    let instance = captures.join("provider");
    fs::create_dir_all(&instance).expect("capture tree");
    for directory in [&debug, &captures, &instance] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o755))
            .expect("permissive fixture");
    }

    write_capture(&job(&session, b"private")).expect("write");

    for directory in [debug, captures, instance] {
        assert_eq!(
            fs::metadata(directory)
                .expect("capture directory")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

/// Proves a symlinked Provider instance sink cannot redirect an attributed
/// capture outside the harness-owned session tree.
#[cfg(unix)]
#[test]
fn rejects_symlinked_instance_sink() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp");
    let session = temp.path().join("session");
    let external = temp.path().join("external");
    fs::create_dir(&session).expect("session");
    fs::create_dir(&external).expect("external");
    fs::create_dir_all(session.join("debug/provider-requests")).expect("parents");
    symlink(&external, session.join("debug/provider-requests/provider")).expect("symlink");
    assert!(write_capture(&job(&session, b"opaque")).is_err());
    assert!(fs::read_dir(external).expect("external").next().is_none());
}

/// Proves missing session roots are never created for late captures.
#[test]
fn rejects_missing_session_root() {
    let temp = TempDir::new().expect("temp");
    let missing = temp.path().join("missing");
    assert!(write_capture(&job(&missing, b"opaque")).is_err());
    assert!(!missing.exists());
}

/// Proves symlinks at the session, debug, or provider-requests boundary cannot
/// redirect opaque bytes outside the harness-owned tree.
#[cfg(unix)]
#[test]
fn rejects_symlinked_capture_ancestors() {
    use std::os::unix::fs::symlink;

    for boundary in ["session", "debug", "provider-requests"] {
        let temp = TempDir::new().expect("temp");
        let external = temp.path().join("external");
        fs::create_dir(&external).expect("external");
        let session = temp.path().join("session");
        match boundary {
            "session" => symlink(&external, &session).expect("session symlink"),
            "debug" => {
                fs::create_dir(&session).expect("session");
                symlink(&external, session.join("debug")).expect("debug symlink");
            }
            "provider-requests" => {
                fs::create_dir_all(session.join("debug")).expect("debug");
                symlink(&external, session.join("debug/provider-requests"))
                    .expect("captures symlink");
            }
            _ => unreachable!(),
        }
        assert!(
            write_capture(&job(&session, b"opaque")).is_err(),
            "{boundary}"
        );
        assert!(
            fs::read_dir(&external).expect("external").next().is_none(),
            "{boundary}"
        );
    }
}

/// Proves harness filesystem admission accepts exactly the production capacity
/// and rejects the next job immediately.
#[test]
fn full_queue_rejects_without_blocking() {
    assert_eq!(CAPTURE_QUEUE_CAPACITY, 64);
    let temp = TempDir::new().expect("temp");
    let (sender, _receiver) = mpsc::sync_channel(CAPTURE_QUEUE_CAPACITY);
    let writer = CaptureWriter::with_sender(sender);
    for index in 1..=64 {
        writer
            .try_submit(job(temp.path(), b"capture"))
            .unwrap_or_else(|_| panic!("admission {index} within production capacity"));
    }
    assert!(matches!(
        writer.try_submit(job(temp.path(), b"overflow")),
        Err(CaptureSubmitError::Full)
    ));
}
