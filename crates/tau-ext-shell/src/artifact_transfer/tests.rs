use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
#[cfg(unix)]
use std::time::Duration;

use super::*;
#[cfg(unix)]
use crate::config::ExtConfig;
#[cfg(unix)]
use crate::runtime::ShellRuntime;
use crate::tool_lifecycle::ToolLifecycleRegistry;

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

    let Ok(Command::StartExport {
        mut upload,
        filename,
        ..
    }) = prepare_export(invoke, lifecycle, tempdir.path())
    else {
        panic!("regular file should prepare an export command");
    };
    assert_eq!(filename.as_deref(), Some("original.bin"));
    let Some(tau_proto::ArtifactOp::Begin { size }) = upload.next_op() else {
        panic!("expected upload begin");
    };
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
    assert_eq!(error.message, "export path is not a regular file");

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
            assert_eq!(error.message, "export path is not a regular file");
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
            invoke,
            lifecycle,
            upload: ArtifactUpload::new(b"x".to_vec()).expect("upload"),
            filename: None,
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
