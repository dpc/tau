use std::io::{Cursor, Error as IoError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::*;
use crate::display::ErrorExplanation;

/// Every accepted tool spelling normalizes before any transfer is scheduled.
#[test]
fn import_argument_spellings_normalize_to_the_same_download() {
    let key = tau_proto::ArtifactKey::parse("0123456789abcdef01234567").expect("canonical key");
    for spelling in [
        key.to_string(),
        tau_proto::artifact_uri(&key),
        tau_proto::artifact_reference(&key),
    ] {
        let (mut invoke, lifecycle, _output, _rx) =
            active_invocation("import-spelling", IMPORT_TOOL_NAME);
        invoke.arguments = CborValue::Map(vec![(
            CborValue::Text("key".into()),
            CborValue::Text(spelling),
        )]);
        let Ok(Command::StartImport { download, .. }) =
            prepare_import_with_limit(invoke, lifecycle, tau_proto::ARTIFACT_MAX_BYTES)
        else {
            panic!("accepted import spelling")
        };
        assert!(
            matches!(download.next_op(), Some(tau_proto::ArtifactOp::Open { key: parsed }) if parsed == key)
        );
    }
}

/// Named imports retain a file kind, private parent ownership, exact metadata,
/// and independent paths; cancellation and undelivered completions clean
/// parents.
#[test]
fn named_imports_restore_metadata_and_clean_private_parents() {
    let identity = tau_proto::ArtifactIdentity::metadata(
        Some("example.txt".into()),
        Some("text/plain".into()),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let limits = tau_artifact::Limits::default();
    let first = write_import(b"same", &cancel, limits, &identity).unwrap();
    let second = write_import(b"same", &cancel, limits, &identity).unwrap();
    let parent = first.path().parent().unwrap().to_owned();
    assert_ne!(first.path(), second.path());
    assert_eq!(first.path().file_name().unwrap(), "example.txt");
    assert!(!first.directory);
    assert_eq!(first.mime_type.as_deref(), Some("text/plain"));
    assert_eq!(fs::read(first.path()).unwrap(), b"same");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(first.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let mut manager = ArtifactTransferManager::unavailable();
    assert!(
        manager
            .control
            .send(Command::ImportWritten {
                call_id: "named".into(),
                size: 4,
                result: Ok(first),
            })
            .is_ok()
    );
    manager.shutdown();
    drop(manager);
    assert!(!parent.exists());
    let second_parent = second.path().parent().unwrap().to_owned();
    drop(second);
    assert!(!second_parent.exists());
    cancel.store(true, Ordering::Release);
    assert!(write_import(b"same", &cancel, limits, &identity).is_err());
    cancel.store(false, Ordering::Release);
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "/absolute",
        "a\\b",
        "a\0b",
        "a\nb",
        "C:drive",
    ] {
        let identity = tau_proto::ArtifactIdentity::metadata(Some(name.into()), None).unwrap();
        let temp = write_import(b"same", &cancel, limits, &identity).unwrap();
        assert!(temp.cleanup_parent.is_none(), "{name:?}");
        assert_eq!(temp.mime_type, None);
    }
}
#[cfg(unix)]
use crate::config::ExtConfig;
#[cfg(unix)]
use crate::runtime::ShellRuntime;
use crate::tool_lifecycle::ToolLifecycleRegistry;

/// Unreported extracted directories are removed recursively; cancellation and
/// malformed typed bytes never return a misleading regular-file success.
#[test]
fn directory_import_cleanup_and_type_identity() {
    let source = tempfile::tempdir().expect("private source");
    fs::write(source.path().join("data"), b"binary\0data").expect("binary source");
    let limits = tau_artifact::Limits::default();
    let bytes = tau_artifact::pack(source.path(), limits, || false).expect("encode fixture");
    let cancel = AtomicBool::new(false);
    let identity = tau_proto::ArtifactIdentity::LegacyPayload;
    let temp = write_import(&bytes, &cancel, limits, &identity).expect("import fixture");
    let path = temp.path().to_owned();
    assert!(temp.directory);
    assert_eq!(
        fs::read(path.join("data")).expect("imported bytes"),
        b"binary\0data"
    );
    drop(temp);
    assert!(!path.exists());
    cancel.store(true, Ordering::Release);
    assert!(write_import(&bytes, &cancel, limits, &identity).is_err());
    cancel.store(false, Ordering::Release);
    assert!(write_import(&bytes[..bytes.len() - 1], &cancel, limits, &identity).is_err());
    let ordinary =
        write_import(b"not a directory", &cancel, limits, &identity).expect("ordinary import");
    assert!(
        !ordinary.directory,
        "only typed bundle bytes select extraction"
    );
}

/// Reservation spans preparation/transfer/write lifetimes without allowing a
/// queue's entry count to multiply a configured large original-byte ceiling.
#[test]
fn original_buffer_reservations_bound_concurrency() {
    let budget = buffer_budget::BufferBudget::default();
    let a = budget.reserve(64 * 1024 * 1024).expect("first reservation");
    let b = budget
        .reserve(64 * 1024 * 1024)
        .expect("second reservation");
    assert!(budget.reserve(1).is_err());
    drop((a, b));
    let large = budget
        .reserve(ARTIFACT_MAX_BYTES)
        .expect("exclusive large reservation");
    assert!(budget.reserve(1).is_err());
    drop(large);
    assert!(budget.reserve(ARTIFACT_MAX_BYTES).is_ok());
}

/// Harness-supplied limits reach off-loop export preparation before upload
/// admission, rather than using the codec's or transport's larger defaults.
#[test]
fn configured_policy_rejects_export_before_rpc() {
    let root = tempfile::tempdir().expect("private source");
    let path = root.path().join("source");
    fs::write(&path, b"two bytes exceed one").expect("source bytes");
    let mut manager = ArtifactTransferManager::unavailable();
    manager.configure_limits(tau_proto::ArtifactLimits {
        stored_bytes: tau_proto::ArtifactSize::new(1).expect("small stored limit"),
        expanded_bytes: tau_proto::ArtifactSize::new(1).expect("small expanded limit"),
    });
    let (invoke, lifecycle, output, rx) = export_preparation_fixture(&path);
    assert!(
        !manager
            .control
            .prepare(invoke, lifecycle, root.path(), &output)
    );
    assert!(
        manager
            .control
            .queue
            .lock()
            .expect("command queue")
            .is_empty()
    );
    let tau_proto::HarnessInputMessage::Emit(emit) = rx.recv().expect("error terminal") else {
        panic!("expected terminal emit");
    };
    assert!(matches!(*emit.event, Event::ToolErrorReported(_)));
}

/// Minimal client extension used to obtain a real production artifact writer.
struct ArtifactClientFixture;

impl tau_client::TauExtension for ArtifactClientFixture {
    type State = ArtifactTransferManager;

    fn name(&self) -> &'static str {
        "artifact-client-fixture"
    }

    fn register(self, _builder: &mut tau_client::ExtensionBuilder<Self::State>) {}
}

/// Thread-safe byte sink retaining complete protocol frames for assertions.
#[derive(Clone, Default)]
struct CapturedWriter {
    /// Bytes successfully accepted before any configured failure.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Whether later writes should fail to close client admission.
    fail: Arc<AtomicBool>,
}

impl std::io::Write for CapturedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.fail.load(Ordering::Acquire) {
            return Err(IoError::other("forced artifact writer failure"));
        }
        self.bytes
            .lock()
            .expect("captured writer")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedWriter {
    /// Waits for and returns the first outbound artifact operation matching
    /// `predicate`.
    fn wait_for_artifact_op(
        &self,
        predicate: impl Fn(&tau_proto::ArtifactOp) -> bool,
    ) -> tau_proto::ArtifactRequest {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let bytes = self.bytes.lock().expect("captured writer").clone();
            let mut reader = tau_proto::HarnessInputReader::new(Cursor::new(bytes));
            while let Ok(Some(message)) = reader.read_message() {
                if let tau_proto::HarnessInputMessage::ArtifactRequest(request) = message
                    && predicate(&request.op)
                {
                    return request;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for artifact operation"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Counts complete outbound artifact operations matching `predicate`.
    fn artifact_op_count(&self, predicate: impl Fn(&tau_proto::ArtifactOp) -> bool) -> usize {
        let bytes = self.bytes.lock().expect("captured writer").clone();
        let mut reader = tau_proto::HarnessInputReader::new(Cursor::new(bytes));
        let mut count = 0;
        while let Ok(Some(message)) = reader.read_message() {
            if let tau_proto::HarnessInputMessage::ArtifactRequest(request) = message
                && predicate(&request.op)
            {
                count += 1;
            }
        }
        count
    }

    /// Makes the production writer reject every later frame.
    fn fail_writes(&self) {
        self.fail.store(true, Ordering::Release);
    }
}

/// Returns a configured real client runtime whose state owns an artifact
/// manager.
fn live_artifact_manager() -> (
    tau_client::ManualExtensionRuntime<ArtifactTransferManager>,
    CapturedWriter,
) {
    let configure = HarnessOutputMessage::Configure(tau_proto::Configure {
        purpose: tau_proto::ConfigurePurpose::Runtime,
        artifact_limits: Default::default(),
        harness_protocol_version: None,
        tool_prefix: None,
        instance_name: "artifact-client-fixture".parse().expect("extension name"),
        config: CborValue::Map(Vec::new()),
        state_dir: None,
        secrets: Default::default(),
        absent_optional_secrets: Default::default(),
        settings_files: Default::default(),
    });
    let mut input = Vec::new();
    tau_proto::HarnessOutputWriter::new(&mut input)
        .write_message(&configure)
        .expect("encode configure");
    let writer = CapturedWriter::default();
    let captured = writer.clone();
    let runtime = tau_client::TauExtensionRunner::new(ArtifactClientFixture)
        .start_manual_loop_with_state(Cursor::new(input), writer, |handle| {
            ArtifactTransferManager::new(ArtifactClient::new(handle))
        })
        .expect("start artifact client fixture");
    (runtime, captured)
}

/// Builds one admitted lifecycle and invocation for direct manager tests.
fn active_invocation(
    call_id: &str,
    tool_name: &str,
) -> (
    ToolStarted,
    ToolLifecycle,
    Output,
    mpsc::Receiver<tau_proto::HarnessInputMessage>,
) {
    let (tx, rx) = mpsc::channel();
    let output = Output::channel(tx);
    let invoke = ToolStarted {
        invocation_policy: Default::default(),
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new(tool_name),
        arguments: CborValue::Map(Vec::new()),
        agent_id: "agent-artifact-error".parse().expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    };
    let lifecycle = ToolLifecycleRegistry::default().admit(
        invoke.call_id.clone(),
        invoke.tool_name.clone(),
        invoke.agent_id.clone(),
        output.clone(),
    );
    assert!(lifecycle.start_effect());
    (invoke, lifecycle, output, rx)
}

/// Extracts the one terminal tool error from a direct output channel.
fn expect_tool_error(rx: &mpsc::Receiver<tau_proto::HarnessInputMessage>) -> tau_proto::ToolError {
    let tau_proto::HarnessInputMessage::Emit(emit) = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("tool terminal")
    else {
        panic!("expected terminal emit");
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("expected tool error");
    };
    error
}

/// Prevents error and cancellation replacements from erasing the transferred
/// file's source, without claiming a successful destination or byte count.
#[test]
fn artifact_failure_displays_preserve_source_context() {
    for (tool, field, source) in [
        (EXPORT_TOOL_NAME, "path", "relative/image.png"),
        (IMPORT_TOOL_NAME, "key", "blake3:unavailable"),
    ] {
        let (mut invoke, lifecycle, output, rx) = export_preparation_fixture(Path::new(source));
        invoke.tool_name = tau_proto::ToolName::new(tool);
        invoke.arguments = CborValue::Map(vec![text_entry(field, source.to_owned())]);
        report_error(
            &output,
            &invoke,
            "storage unavailable\nmore detail".to_owned(),
        );
        let error = expect_tool_error(&rx);
        let display = error.display.expect("error display");
        assert_eq!(display.args, source);
        assert_eq!(display.status, tau_proto::ToolUseStatus::Error);
        assert_eq!(display.status_text, "storage unavailable");
        assert_eq!(display.stats.bytes, None);
        assert_eq!(display.payload, None);

        report_cancelled(&output, invoke, lifecycle);
        let tau_proto::HarnessInputMessage::Emit(emit) = rx.recv().expect("cancel terminal") else {
            panic!("expected terminal emit");
        };
        let Event::ToolCancelledReported(cancelled) = *emit.event else {
            panic!("expected tool cancellation");
        };
        let display = cancelled.display.expect("cancel display");
        assert_eq!(display.args, source);
        assert_eq!(display.status, tau_proto::ToolUseStatus::Warning);
        assert_eq!(display.status_text, "cancelled");
        assert_eq!(display.stats.bytes, None);
        assert_eq!(display.payload, None);
    }
}

/// Builds one export invocation and admitted lifecycle for direct preparation
/// coverage.
fn export_preparation_fixture(
    path: &Path,
) -> (
    ToolStarted,
    ToolLifecycle,
    Output,
    mpsc::Receiver<tau_proto::HarnessInputMessage>,
) {
    let (tx, rx) = mpsc::channel();
    let output = Output::channel(tx);
    let invoke = ToolStarted {
        invocation_policy: Default::default(),
        call_id: "export-preparation".into(),
        tool_name: tau_proto::ToolName::new(EXPORT_TOOL_NAME),
        arguments: CborValue::Map(vec![(
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        )]),
        agent_id: "agent-export".parse().expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    };
    let lifecycle = ToolLifecycleRegistry::default().admit(
        invoke.call_id.clone(),
        invoke.tool_name.clone(),
        invoke.agent_id.clone(),
        output.clone(),
    );
    assert!(lifecycle.start_effect());
    (invoke, lifecycle, output, rx)
}

/// Ensures ordinary regular files still preserve their exact bytes through
/// export preparation.
#[test]
fn regular_file_export_preparation_preserves_bytes() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let path = tempdir.path().join("original.bin");
    let expected = b"artifact bytes".to_vec();
    fs::write(&path, &expected).expect("write fixture");
    let (invoke, lifecycle, _output, _rx) = export_preparation_fixture(&path);

    let Ok(Command::StartExport { mut upload, .. }) =
        prepare_export(invoke, lifecycle, tempdir.path())
    else {
        panic!("regular file should prepare an export command");
    };
    let Some(tau_proto::ArtifactOp::Begin { size, identity }) = upload.next_op() else {
        panic!("expected upload begin");
    };
    assert_eq!(identity.filename(), Some("original.bin"));
    assert_eq!(size.get(), expected.len() as u64);
    upload
        .accept(tau_proto::ArtifactValue::Upload {
            upload: "regular-upload".parse().expect("upload id"),
        })
        .expect("accept upload");
    let Some(tau_proto::ArtifactOp::Write { bytes, .. }) = upload.next_op() else {
        panic!("expected upload write");
    };
    assert_eq!(bytes, expected);
}

/// The shell boundary must reject directory-root links even when trailing slash
/// or dot spellings would bypass a naive final-component NOFOLLOW open.
#[cfg(unix)]
#[test]
fn directory_root_symlink_spellings_are_rejected() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("source fixture");
    fs::create_dir(root.path().join("target")).expect("target directory");
    symlink("target", root.path().join("link")).expect("root link");
    for name in ["link", "link/", "link/.", "link/./"] {
        let (invoke, lifecycle, _output, _rx) = export_preparation_fixture(&root.path().join(name));
        assert!(
            prepare_export(invoke, lifecycle, root.path()).is_err(),
            "{name}"
        );
    }
}

/// Ensures Unix exports continue to follow a symlink whose opened target is a
/// regular file.
#[cfg(unix)]
#[test]
fn symlink_to_regular_file_export_preparation_preserves_bytes() {
    use std::os::unix::fs::symlink;

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let target = tempdir.path().join("target.bin");
    let link = tempdir.path().join("linked.bin");
    let expected = b"linked artifact bytes".to_vec();
    fs::write(&target, &expected).expect("write target");
    symlink("target.bin", &link).expect("symlink target");
    let (invoke, lifecycle, _output, _rx) = export_preparation_fixture(&link);

    let Ok(Command::StartExport { mut upload, .. }) =
        prepare_export(invoke, lifecycle, tempdir.path())
    else {
        panic!("linked regular file should prepare an export command");
    };
    upload
        .accept(tau_proto::ArtifactValue::Upload {
            upload: "symlink-upload".parse().expect("upload id"),
        })
        .expect("accept upload");
    let Some(tau_proto::ArtifactOp::Write { bytes, .. }) = upload.next_op() else {
        panic!("expected upload write");
    };
    assert_eq!(bytes, expected);
}

/// Runs the FIFO rejection through direct preparation and the real scheduled
/// runtime path. The parent test isolates this helper so an old blocking open
/// can be killed instead of hanging the test binary.
#[cfg(unix)]
#[test]
#[ignore = "subprocess helper"]
fn fifo_export_rejection_subprocess_helper() {
    let path = std::env::var_os("TAU_EXPORT_FIFO_PATH")
        .map(PathBuf::from)
        .expect("FIFO path");
    let (invoke, lifecycle, output, rx) = export_preparation_fixture(&path);
    let manager = ArtifactTransferManager::unavailable();

    assert!(
        !manager
            .control()
            .prepare(invoke, lifecycle, Path::new("/"), &output)
    );
    assert!(
        manager
            .control
            .queue
            .lock()
            .expect("artifact queue")
            .is_empty(),
        "rejected FIFO must not queue StartExport"
    );
    let tau_proto::HarnessInputMessage::Emit(emit) = rx.recv().expect("preparation error") else {
        panic!("expected terminal emit");
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("expected preparation error");
    };
    assert_eq!(
        error.explanation(),
        "export path is not a regular file or directory"
    );

    let (tx, runtime_rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new_for_test_harness(
        Output::channel(tx),
        ExtConfig::default(),
        crate::DiscoverySourcePolicy::Environment,
        PathBuf::from("/"),
    );
    let scheduled = ToolStarted {
        invocation_policy: Default::default(),
        call_id: "scheduled-fifo-export".into(),
        tool_name: tau_proto::ToolName::new(EXPORT_TOOL_NAME),
        arguments: CborValue::Map(vec![(
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        )]),
        agent_id: "agent-scheduled-export".parse().expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    };
    let local_name = scheduled.tool_name.clone();
    runtime
        .handle_scoped_tool_started(scheduled, &local_name)
        .expect("schedule FIFO export");
    loop {
        let tau_proto::HarnessInputMessage::Emit(emit) = runtime_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("scheduled FIFO terminal")
        else {
            continue;
        };
        if let Event::ToolErrorReported(error) = *emit.event {
            assert_eq!(error.call_id.as_str(), "scheduled-fifo-export");
            assert_eq!(
                error.explanation(),
                "export path is not a regular file or directory"
            );
            break;
        }
    }
    runtime.final_shutdown();
}

/// Ensures a FIFO with no writer, including through a symlink, is rejected
/// promptly without queuing an upload or retaining a worker during final
/// shutdown.
#[cfg(unix)]
#[test]
fn fifo_exports_reject_without_blocking_worker_or_final_shutdown() {
    use std::os::unix::fs::symlink;
    use std::process::{Command as ProcessCommand, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let fifo = tempdir.path().join("artifact.fifo");
    let status = ProcessCommand::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo failed");
    let link = tempdir.path().join("artifact-link.fifo");
    symlink("artifact.fifo", &link).expect("symlink FIFO");

    for path in [&fifo, &link] {
        let mut child = ProcessCommand::new(std::env::current_exe().expect("current test binary"))
            .arg("--ignored")
            .arg("--exact")
            .arg("artifact_transfer::tests::fifo_export_rejection_subprocess_helper")
            .arg("--nocapture")
            .env("TAU_EXPORT_FIFO_PATH", path)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn FIFO export helper");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().expect("poll FIFO export helper") {
                assert!(status.success(), "FIFO export helper failed: {status}");
                break;
            }
            if Instant::now() >= deadline {
                child.kill().expect("kill blocked FIFO export helper");
                let _ = child.wait();
                panic!("FIFO export helper blocked for {}", path.display());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Builds one active export transfer for main-loop response rejection tests.
fn export_transfer_fixture() -> (
    ArtifactTransferManager,
    WorkScheduler,
    mpsc::Receiver<tau_proto::HarnessInputMessage>,
    tau_proto::ArtifactRequestId,
) {
    let mut manager = ArtifactTransferManager::unavailable();
    let scheduler = WorkScheduler::new(Default::default());
    let (tx, rx) = mpsc::channel();
    let output = Output::channel(tx);
    let invoke = ToolStarted {
        invocation_policy: Default::default(),
        call_id: "artifact-call".into(),
        tool_name: tau_proto::ToolName::new(EXPORT_TOOL_NAME),
        arguments: CborValue::Map(Vec::new()),
        agent_id: "agent-a".parse().expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    };
    let lifecycle = ToolLifecycleRegistry::default().admit(
        invoke.call_id.clone(),
        invoke.tool_name.clone(),
        invoke.agent_id.clone(),
        output,
    );
    assert!(lifecycle.start_effect());
    manager.transfers.insert(
        invoke.call_id.clone(),
        Transfer::Export {
            reservation: None,
            directory: false,
            invoke,
            lifecycle,
            upload: ArtifactUpload::new(b"x".to_vec()).expect("upload"),
        },
    );
    let request_id: tau_proto::ArtifactRequestId =
        "oversized-response".parse().expect("request id");
    manager
        .requests
        .insert(request_id.clone(), "artifact-call".into());
    (manager, scheduler, rx, request_id)
}

/// Ensures imported originals cannot bypass the dedicated queued/running byte
/// budget even though their scheduler metadata excludes already-charged bytes.
#[test]
fn import_write_budget_rejects_bytes_above_aggregate_limit() {
    let budget = ImportWriteBudget::default();
    let reservation = budget
        .reserve(IMPORT_WRITE_BYTES_LIMIT)
        .expect("exact limit");
    assert!(budget.reserve(1).is_err());
    drop(reservation);
    assert!(budget.reserve(1).is_ok());
}

/// Ensures cancellation observed by the temp writer after creation removes the
/// retained file instead of returning an unreported local path.
#[test]
fn cancelled_private_temp_write_returns_no_path() {
    let cancelled = AtomicBool::new(true);
    assert!(write_private_temp(b"original", &cancelled).is_err());
}

/// Export can preserve an explicit declaration even when the extension differs;
/// absent declarations only infer known extensions and never invent unknown
/// types.
#[test]
fn export_artifact_media_hints_preserve_explicit_claims_and_bound_them() {
    let (mut invoke, _lifecycle, _output, _rx) = active_invocation("media", EXPORT_TOOL_NAME);
    assert_eq!(
        hints::export_mime(&invoke, Some("photo.JPEG")),
        Some("image/jpeg".into())
    );
    assert_eq!(hints::export_mime(&invoke, Some("unknown.custom")), None);
    assert_eq!(hints::export_mime(&invoke, None), None);
    invoke.arguments = CborValue::Map(vec![text_entry("mime_type", "custom/type".into())]);
    assert_eq!(
        hints::export_mime(&invoke, Some("photo.png")),
        Some("custom/type".into())
    );
    invoke.arguments = CborValue::Map(vec![text_entry("mime_type", "é".repeat(200))]);
    let hint = hints::export_mime(&invoke, None).expect("bounded declaration");
    assert!(hint.len() <= 255);
    assert!(hint.is_char_boundary(hint.len()));
}

/// Repeated imports of identical bytes create distinct private generic files,
/// without recovering a filename from shared storage.
#[test]
fn import_temp_names_remain_unique_and_private() {
    let cancel = AtomicBool::new(false);
    let a = write_private_temp(b"same", &cancel).expect("first");
    let b = write_private_temp(b"same", &cancel).expect("second");
    let a_path = a.path.as_ref().expect("path");
    let b_path = b.path.as_ref().expect("path");
    assert_ne!(a_path, b_path);
    for path in [a_path, b_path] {
        assert_eq!(fs::read(path).expect("bytes"), b"same");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

/// Ensures a completed temp write queued immediately before disconnect remains
/// cleanup-owned even when the main loop never drains its completion.
#[test]
fn queued_import_completion_unlinks_unreported_temp_on_shutdown_drop() {
    let mut manager = ArtifactTransferManager::unavailable();
    let temp = tempfile::NamedTempFile::new().expect("temp");
    let (_, path) = temp.keep().expect("retain temp");
    assert!(
        manager
            .control
            .send(Command::ImportWritten {
                call_id: "finished-import".into(),
                size: 0,
                result: Ok(RetainedTemp {
                    cleanup_parent: None,
                    mime_type: None,
                    reservation: None,
                    directory: false,
                    path: Some(path.clone()),
                }),
            })
            .is_ok()
    );
    manager.shutdown();
    assert!(path.exists(), "queued command still owns the file");
    drop(manager);
    assert!(
        !path.exists(),
        "dropping the undrained completion must unlink"
    );
}

/// Ensures a full completion queue never blocks a worker and a rejected
/// completion immediately drops its retained-temp cleanup guard.
#[test]
fn full_artifact_completion_queue_rejects_without_blocking_or_leaking() {
    let manager = ArtifactTransferManager::unavailable();
    for index in 0..ARTIFACT_COMMAND_LIMIT {
        assert!(
            manager
                .control
                .send(Command::ImportWritten {
                    call_id: format!("queued-{index}").into(),
                    size: 0,
                    result: Err("fixture".to_owned()),
                })
                .is_ok()
        );
    }
    let temp = tempfile::NamedTempFile::new().expect("temp");
    let (_, path) = temp.keep().expect("retain temp");
    let rejected = manager.control.send(Command::ImportWritten {
        call_id: "rejected".into(),
        size: 0,
        result: Ok(RetainedTemp {
            cleanup_parent: None,
            mime_type: None,
            reservation: None,
            directory: false,
            path: Some(path.clone()),
        }),
    });
    assert!(rejected.is_err());
    drop(rejected);
    assert!(!path.exists());
}

/// Ensures an oversized response with the exact outstanding correlation settles
/// the tool terminally instead of leaving the transfer pinned indefinitely.
#[test]
fn oversized_matching_response_settles_transfer_but_stale_response_is_ignored() {
    let (mut manager, scheduler, rx, request_id) = export_transfer_fixture();
    let (output_tx, output_rx) = mpsc::channel();
    let output = Output::channel(output_tx);
    manager.handle_result(
        ArtifactResult {
            request_id,
            result: Ok(tau_proto::ArtifactValue::Chunk {
                offset: 0,
                bytes: vec![0; tau_proto::ARTIFACT_FRAME_BYTES],
                eof: true,
            }),
        },
        &scheduler,
        &output,
    );
    assert!(manager.transfers.is_empty());
    let tau_proto::HarnessInputMessage::Emit(emit) = output_rx.recv().expect("terminal error")
    else {
        panic!("expected terminal emit");
    };
    assert!(matches!(*emit.event, Event::ToolErrorReported(_)));

    manager.handle_result(
        ArtifactResult {
            request_id: "stale-response".parse().expect("request id"),
            result: Ok(tau_proto::ArtifactValue::Chunk {
                offset: 0,
                bytes: vec![0; tau_proto::ARTIFACT_FRAME_BYTES],
                eof: true,
            }),
        },
        &scheduler,
        &output,
    );
    assert!(output_rx.try_recv().is_err());
    drop(rx);
}

/// Ensures a correlated terminal upload error releases its known server upload
/// with one real Abort while preserving the original one-error outcome.
#[test]
fn correlated_upload_error_aborts_known_upload_once() {
    let (mut runtime, captured) = live_artifact_manager();
    let manager = runtime.state_mut();
    manager.bind_session("artifact-session".parse().expect("session id"));
    let (invoke, lifecycle, output, rx) = active_invocation("upload-error", EXPORT_TOOL_NAME);
    let mut upload = ArtifactUpload::new(b"x".to_vec()).expect("upload");
    upload
        .accept(tau_proto::ArtifactValue::Upload {
            upload: "known-upload".parse().expect("upload id"),
        })
        .expect("accept upload");
    let request_id: ArtifactRequestId = "upload-error-request".parse().expect("request id");
    manager
        .requests
        .insert(request_id.clone(), invoke.call_id.clone());
    manager.transfers.insert(
        invoke.call_id.clone(),
        Transfer::Export {
            reservation: None,
            directory: false,
            invoke,
            lifecycle,
            upload,
        },
    );

    manager.handle_result(
        ArtifactResult {
            request_id: request_id.clone(),
            result: Err(ArtifactError::Busy),
        },
        &WorkScheduler::new(Default::default()),
        &output,
    );

    let release = captured.wait_for_artifact_op(
        |op| matches!(op, tau_proto::ArtifactOp::Abort { upload } if upload.as_str() == "known-upload"),
    );
    assert!(manager.transfers.is_empty());
    assert!(manager.requests.is_empty());
    assert_eq!(
        expect_tool_error(&rx).explanation(),
        "artifact storage is busy; retry this tool call"
    );
    manager.handle_result(
        ArtifactResult {
            request_id,
            result: Err(ArtifactError::Busy),
        },
        &WorkScheduler::new(Default::default()),
        &output,
    );
    manager.handle_result(
        ArtifactResult {
            request_id: release.request_id,
            result: Ok(tau_proto::ArtifactValue::Done),
        },
        &WorkScheduler::new(Default::default()),
        &output,
    );
    manager.cancel(&"upload-error".into(), &output);
    manager.shutdown();
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    runtime.finish().expect("finish client fixture");
    assert_eq!(
        captured.artifact_op_count(
            |op| matches!(op, tau_proto::ArtifactOp::Abort { upload } if upload.as_str() == "known-upload")
        ),
        1
    );
}

/// Ensures a correlated terminal download error releases its known server read
/// with one real Close and never publishes imported bytes.
#[test]
fn correlated_download_error_closes_known_read_once() {
    let (mut runtime, captured) = live_artifact_manager();
    let manager = runtime.state_mut();
    manager.bind_session("artifact-session".parse().expect("session id"));
    let (invoke, lifecycle, output, rx) = active_invocation("download-error", IMPORT_TOOL_NAME);
    let key =
        tau_proto::ArtifactKey::from_digest(blake3::hash(b"x").to_hex().as_str()).expect("key");
    let descriptor = tau_proto::ArtifactDescriptor::new(key.clone(), 1).expect("descriptor");
    let mut download = ArtifactDownload::new(key);
    download
        .accept(tau_proto::ArtifactValue::Opened {
            read: "known-read".parse().expect("read id"),
            descriptor,
        })
        .expect("accept opened");
    let request_id: ArtifactRequestId = "download-error-request".parse().expect("request id");
    manager
        .requests
        .insert(request_id.clone(), invoke.call_id.clone());
    manager.transfers.insert(
        invoke.call_id.clone(),
        Transfer::Import {
            reservation: None,
            invoke,
            lifecycle,
            download,
        },
    );

    manager.handle_result(
        ArtifactResult {
            request_id,
            result: Err(ArtifactError::Io),
        },
        &WorkScheduler::new(Default::default()),
        &output,
    );

    captured.wait_for_artifact_op(
        |op| matches!(op, tau_proto::ArtifactOp::Close { read } if read.as_str() == "known-read"),
    );
    assert_eq!(
        expect_tool_error(&rx).explanation(),
        "artifact storage I/O failed"
    );
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    runtime.finish().expect("finish client fixture");
    assert_eq!(
        captured.artifact_op_count(
            |op| matches!(op, tau_proto::ArtifactOp::Close { read } if read.as_str() == "known-read")
        ),
        1
    );
}

/// Ensures a later local admission failure drops the transfer only after a real
/// best-effort Abort attempt, while an initial failure invents no identity.
#[test]
fn local_request_failure_releases_only_known_transfer_identity() {
    let (mut runtime, captured) = live_artifact_manager();
    let manager = runtime.state_mut();
    manager.bind_session("artifact-session".parse().expect("session id"));
    let (invoke, lifecycle, output, rx) = active_invocation("local-error", EXPORT_TOOL_NAME);
    let mut upload = ArtifactUpload::new(b"x".to_vec()).expect("upload");
    upload
        .accept(tau_proto::ArtifactValue::Upload {
            upload: "local-known-upload".parse().expect("upload id"),
        })
        .expect("accept upload");
    manager.transfers.insert(
        invoke.call_id.clone(),
        Transfer::Export {
            reservation: None,
            directory: false,
            invoke: invoke.clone(),
            lifecycle,
            upload,
        },
    );
    manager.reject_next_request = true;

    manager.start_next(&invoke.call_id, &output);

    captured.wait_for_artifact_op(
        |op| matches!(op, tau_proto::ArtifactOp::Abort { upload } if upload.as_str() == "local-known-upload"),
    );
    assert!(manager.transfers.is_empty());
    assert_eq!(
        expect_tool_error(&rx).explanation(),
        "artifact request failed: tau client detached FIFO or frame byte limit is exhausted"
    );

    let (initial, initial_lifecycle, initial_output, initial_rx) =
        active_invocation("initial-error", EXPORT_TOOL_NAME);
    manager.transfers.insert(
        initial.call_id.clone(),
        Transfer::Export {
            reservation: None,
            directory: false,
            invoke: initial.clone(),
            lifecycle: initial_lifecycle,
            upload: ArtifactUpload::new(b"x".to_vec()).expect("upload"),
        },
    );
    manager.reject_next_request = true;
    manager.start_next(&initial.call_id, &initial_output);
    assert_eq!(
        expect_tool_error(&initial_rx).explanation(),
        "artifact request failed: tau client detached FIFO or frame byte limit is exhausted"
    );
    runtime.finish().expect("finish client fixture");
    assert_eq!(
        captured.artifact_op_count(|op| matches!(op, tau_proto::ArtifactOp::Abort { .. })),
        1,
        "an initial Begin failure must not invent an upload identity"
    );
}

/// Ensures failure to admit best-effort cleanup cannot replace or duplicate the
/// original correlated tool error.
#[test]
fn cleanup_admission_failure_preserves_original_terminal() {
    let (mut runtime, captured) = live_artifact_manager();
    captured.fail_writes();
    let handle = runtime.handle();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let request = tau_proto::ArtifactRequest {
            request_id: "force-writer-close".parse().expect("request id"),
            expected_session_id: "artifact-session".parse().expect("session id"),
            op: tau_proto::ArtifactOp::Available,
        };
        if handle
            .send_detached(tau_proto::HarnessInputMessage::ArtifactRequest(request))
            .is_err()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "client writer did not close after forced failure"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let manager = runtime.state_mut();
    manager.bind_session("artifact-session".parse().expect("session id"));
    let (invoke, lifecycle, output, rx) = active_invocation("cleanup-fails", EXPORT_TOOL_NAME);
    let mut upload = ArtifactUpload::new(b"x".to_vec()).expect("upload");
    upload
        .accept(tau_proto::ArtifactValue::Upload {
            upload: "cleanup-fails-upload".parse().expect("upload id"),
        })
        .expect("accept upload");
    let request_id: ArtifactRequestId = "cleanup-fails-request".parse().expect("request id");
    manager
        .requests
        .insert(request_id.clone(), invoke.call_id.clone());
    manager.transfers.insert(
        invoke.call_id.clone(),
        Transfer::Export {
            reservation: None,
            directory: false,
            invoke,
            lifecycle,
            upload,
        },
    );

    manager.handle_result(
        ArtifactResult {
            request_id,
            result: Err(ArtifactError::Integrity),
        },
        &WorkScheduler::new(Default::default()),
        &output,
    );

    assert_eq!(
        expect_tool_error(&rx).explanation(),
        "artifact size or digest verification failed"
    );
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    let _ = runtime.finish();
}
