use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

use super::*;
use crate::tool_lifecycle::ToolLifecycleRegistry;

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
