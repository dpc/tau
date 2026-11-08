//! Captured-policy prepared upload and cancellation tests using real SDK
//! frames.

use std::io::{BufReader, Cursor};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tau_client::{ClientHandle, ExtensionBuilder, TauExtension, TauExtensionRunner};
use tau_proto::{
    ArtifactDescriptor, ArtifactError, ArtifactKey, ArtifactOp, ArtifactResult, ArtifactValue,
    CborValue, Event, HarnessInputMessage, HarnessOutputMessage, PreparedImageStorage, ToolCallId,
    ToolCancelled, ToolResultContentPart, ToolStarted, ToolType,
};

use crate::artifact_image::{ActiveCall, ArtifactImageManager, DecodeCompletion, MAX_ACTIVE_READS};
use crate::read_image::{ReadImageOutput, ReadImageRequest};
use crate::tests::SharedWriter;

/// Transport-only fixture; tests never access a filesystem or real harness.
struct TestExtension;

impl TauExtension for TestExtension {
    type State = ();

    fn name(&self) -> &'static str {
        "image-upload-test"
    }

    fn register(self, _: &mut ExtensionBuilder<Self::State>) {}
}

/// Runs private manager transitions while exercising the real configured
/// writer.
fn with_manager(f: impl FnOnce(&mut ArtifactImageManager)) -> Vec<HarnessInputMessage> {
    let mut input = Vec::new();
    tau_proto::HarnessOutputWriter::new(&mut input)
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            artifact_limits: Default::default(),
            purpose: Default::default(),
            harness_protocol_version: Some(tau_proto::PROTOCOL_VERSION),
            config: CborValue::Null,
            instance_name: "image-upload-test".parse().expect("instance"),
            tool_prefix: None,
            state_dir: None,
            secrets: Default::default(),
            absent_optional_secrets: Default::default(),
            settings_files: Default::default(),
        }))
        .expect("configure");
    let output = SharedWriter::default();
    let runtime = TauExtensionRunner::new(TestExtension)
        .start_manual_loop(Cursor::new(input), output.clone(), ())
        .expect("runtime");
    let handle: ClientHandle = runtime.handle();
    let mut manager = ArtifactImageManager::new(handle);
    manager.bind_session("session-old".parse().expect("session"));
    f(&mut manager);
    manager.shutdown_transport();
    drop(manager);
    runtime.finish().expect("finish writer");
    let bytes = output.into_bytes();
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(bytes.as_slice()));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("frame") {
        frames.push(frame);
    }
    frames
}

/// Gives the source and prepared image intentionally different
/// dimensions/bytes.
fn prepared_fixture(
    policy: PreparedImageStorage,
) -> (ToolStarted, ReadImageOutput, Vec<u8>, ArtifactKey) {
    let original = source_bytes();
    let original_key = descriptor(&original).key;
    let invoke = ToolStarted {
        call_id: ToolCallId::new("prepared-call"),
        tool_name: tau_proto::ToolName::new("read_image"),
        agent_id: tau_proto::AgentId::parse("agent").expect("agent"),
        arguments: tau_proto::json_to_cbor(&serde_json::json!({
            "key": original_key.as_str(),
            "region": {"x":0, "y":0, "width":2, "height":1},
        })),
        originator: Default::default(),
        invocation_policy: tau_proto::ToolInvocationPolicy {
            prepared_image_storage: policy,
            ..Default::default()
        },
    };
    let output = ReadImageRequest::from_arguments(&invoke.arguments)
        .expect("request")
        .prepare(&original, original.len() as u64)
        .expect("prepare");
    let prepared = output.provider_content[0]
        .inline_image()
        .expect("prepared inline")
        .data
        .to_vec();
    assert_ne!(descriptor(&prepared).key, original_key);
    (invoke, output, prepared, original_key)
}

/// Encodes a deterministic original independently of its cropped prepared
/// image.
fn source_bytes() -> Vec<u8> {
    let mut encoded = Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(4, 3)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .expect("source PNG");
    encoded.into_inner()
}

/// Produces the exact immutable descriptor expected by the upload state
/// machine.
fn descriptor(bytes: &[u8]) -> ArtifactDescriptor {
    ArtifactDescriptor::new(
        ArtifactKey::from_digest(blake3::hash(bytes).to_hex().as_str()).expect("key"),
        bytes.len() as u64,
    )
    .expect("descriptor")
}

/// Captures the sole sequential foreground request for one synthetic
/// invocation.
fn request_id(
    manager: &ArtifactImageManager,
    call_id: &ToolCallId,
) -> tau_proto::ArtifactRequestId {
    let ids = manager
        .requests
        .iter()
        .filter(|(_, request)| &request.call_id == call_id)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 1);
    ids[0].clone()
}

/// Replies through actual correlation, rather than directly advancing an
/// uploader.
fn respond(
    manager: &mut ArtifactImageManager,
    call_id: &ToolCallId,
    result: Result<ArtifactValue, ArtifactError>,
) {
    manager.handle_result(ArtifactResult {
        request_id: request_id(manager, call_id),
        result,
    });
}

/// Starts at the completed bounded decoder boundary with captured invocation
/// policy.
fn complete_decode(
    manager: &mut ArtifactImageManager,
    invoke: ToolStarted,
    output: ReadImageOutput,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    manager.calls.insert(
        invoke.call_id.clone(),
        ActiveCall::Decoding {
            invoke: invoke.clone(),
            cancelled: Arc::clone(&cancelled),
        },
    );
    manager.decode_active = true;
    manager
        .completions
        .lock()
        .expect("completions")
        .push_back(DecodeCompletion {
            call_id: invoke.call_id,
            cancelled,
            result: Ok(output),
        });
    manager.drain();
}

/// Responds to Begin and verifies every prepared chunk before Finalize is
/// admitted.
fn write_prepared(manager: &mut ArtifactImageManager, call_id: &ToolCallId, expected: &[u8]) {
    respond(
        manager,
        call_id,
        Ok(ArtifactValue::Upload {
            upload: "prepared-upload".parse().expect("upload"),
        }),
    );
    let mut collected = Vec::new();
    loop {
        match manager
            .calls
            .get(call_id)
            .expect("call")
            .next_op()
            .expect("operation")
        {
            ArtifactOp::Write { offset, bytes, .. } => {
                assert_eq!(offset, collected.len() as u64);
                assert!(bytes.len() <= tau_proto::ARTIFACT_CHUNK_BYTES);
                collected.extend_from_slice(&bytes);
                respond(
                    manager,
                    call_id,
                    Ok(ArtifactValue::Written {
                        next_offset: collected.len() as u64,
                    }),
                );
            }
            ArtifactOp::Finalize { .. } => break,
            _ => panic!("upload sequence"),
        }
    }
    assert_eq!(collected, expected);
}

/// Filters ordinary reported terminals without interpreting transfer frames.
fn terminals(frames: &[HarnessInputMessage]) -> Vec<&Event> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit)
                if matches!(
                    emit.event.as_ref(),
                    Event::ToolResultReported(_)
                        | Event::ToolErrorReported(_)
                        | Event::ToolCancelledReported(_)
                ) =>
            {
                Some(emit.event.as_ref())
            }
            _ => None,
        })
        .collect()
}

/// Artifact mode reports only the prepared digest after verified Finalize; the
/// source key and bytes are not substituted for transformed canonical content.
#[test]
fn prepared_upload_reports_exact_reference_only_after_finalize() {
    let (invoke, output, prepared, original) = prepared_fixture(PreparedImageStorage::Artifact);
    let expected = descriptor(&prepared);
    let frames = with_manager(|manager| {
        complete_decode(manager, invoke.clone(), output);
        assert!(matches!(
            manager.calls[&invoke.call_id],
            ActiveCall::Uploading { .. }
        ));
        write_prepared(manager, &invoke.call_id, &prepared);
        respond(
            manager,
            &invoke.call_id,
            Ok(ArtifactValue::Descriptor(expected.clone())),
        );
        assert!(manager.calls.is_empty());
        assert!(manager.requests.is_empty());
    });
    let terminal = terminals(&frames);
    assert_eq!(terminal.len(), 1);
    let Event::ToolResultReported(result) = terminal[0] else {
        panic!("success");
    };
    let [ToolResultContentPart::ImageReference(reference)] = result.provider_content.as_slice()
    else {
        panic!("reference only");
    };
    assert_eq!(reference.blob, expected);
    assert_ne!(reference.blob.key, original);
    assert_eq!((reference.width, reference.height), (2, 1));
    assert_eq!(result.call_id, invoke.call_id);
    let finalization = frames.iter().position(|frame| matches!(frame,
        HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Finalize { .. }))).expect("finalize");
    let terminal_position = frames.iter().position(|frame| matches!(frame,
        HarnessInputMessage::Emit(emit) if matches!(emit.event.as_ref(), Event::ToolResultReported(_)))).expect("terminal");
    assert!(finalization < terminal_position);
}

/// Default/Inline output never starts a prepared upload, even with artifact
/// access.
#[test]
fn inline_policy_preserves_typed_bytes_without_prepared_storage() {
    let (invoke, output, prepared, _) = prepared_fixture(PreparedImageStorage::Inline);
    let frames = with_manager(|manager| complete_decode(manager, invoke, output));
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, HarnessInputMessage::ArtifactRequest(_)))
    );
    let terminal = terminals(&frames);
    let [Event::ToolResultReported(result)] = terminal.as_slice() else {
        panic!("one result");
    };
    assert_eq!(
        result.provider_content[0]
            .inline_image()
            .expect("inline")
            .data
            .as_ref(),
        prepared
    );
}

/// Wrong digests or sizes fail the tool without silently falling back to
/// inline.
#[test]
fn prepared_upload_rejects_wrong_final_descriptor_without_fallback() {
    for wrong_size in [false, true] {
        let (invoke, output, prepared, _) = prepared_fixture(PreparedImageStorage::Artifact);
        let mut wrong = descriptor(&prepared);
        if wrong_size {
            wrong.size = tau_proto::ArtifactSize::new(prepared.len() as u64 + 1).expect("size");
        } else {
            let mut changed = prepared.clone();
            changed[0] ^= 1;
            wrong = descriptor(&changed);
        }
        let frames = with_manager(|manager| {
            complete_decode(manager, invoke.clone(), output);
            write_prepared(manager, &invoke.call_id, &prepared);
            let id = request_id(manager, &invoke.call_id);
            manager.handle_result(ArtifactResult {
                request_id: id.clone(),
                result: Ok(ArtifactValue::Descriptor(wrong.clone())),
            });
            manager.handle_result(ArtifactResult {
                request_id: id,
                result: Ok(ArtifactValue::Descriptor(wrong)),
            });
            assert!(manager.calls.is_empty());
        });
        assert!(matches!(
            terminals(&frames).as_slice(),
            [Event::ToolErrorReported(_)]
        ));
        assert!(frames.iter().any(|frame| matches!(frame,
            HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Abort { .. }))));
    }
}

/// Builds the harness-owned cancellation corresponding to one captured call.
fn cancellation(invoke: &ToolStarted) -> ToolCancelled {
    ToolCancelled {
        output: None,
        captured_output: None,
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        tool_type: ToolType::Function,
        presentation: Default::default(),
        display: None,
    }
}

/// Cancellation at allocation, writing, or finalization emits one terminal;
/// a late successful Finalize remains an unknown shared-object outcome.
#[test]
fn prepared_upload_cancellation_retires_every_transfer_stage() {
    for stage in 0..3 {
        let (invoke, output, prepared, _) = prepared_fixture(PreparedImageStorage::Artifact);
        let frames = with_manager(|manager| {
            complete_decode(manager, invoke.clone(), output);
            match stage {
                0 => {}
                1 => respond(
                    manager,
                    &invoke.call_id,
                    Ok(ArtifactValue::Upload {
                        upload: "prepared-upload".parse().expect("upload"),
                    }),
                ),
                _ => write_prepared(manager, &invoke.call_id, &prepared),
            }
            let id = request_id(manager, &invoke.call_id);
            manager.cancel(&cancellation(&invoke));
            manager.cancel(&cancellation(&invoke));
            assert!(manager.calls.is_empty());
            assert!(manager.requests.is_empty());
            assert_eq!(manager.cleanup_allocations.len(), usize::from(stage == 0));
            let late = match stage {
                0 => ArtifactValue::Upload {
                    upload: "late-upload".parse().expect("upload"),
                },
                1 => ArtifactValue::Written {
                    next_offset: prepared.len() as u64,
                },
                _ => ArtifactValue::Descriptor(descriptor(&prepared)),
            };
            manager.handle_result(ArtifactResult {
                request_id: id,
                result: Ok(late),
            });
            assert!(manager.cleanup_allocations.is_empty());
        });
        assert!(matches!(
            terminals(&frames).as_slice(),
            [Event::ToolCancelledReported(_)]
        ));
        assert_eq!(frames.iter().filter(|frame| matches!(frame,
            HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Abort { .. }))).count(), 1);
        assert_eq!(frames.iter().filter(|frame| matches!(frame,
            HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Begin { .. }))).count(), 1);
    }
}

/// Shared-store overload fails only this tool, never retries Begin or emits
/// bytes inline.
#[test]
fn prepared_upload_busy_fails_once_without_inline_fallback() {
    for stage in 0..3 {
        let (invoke, output, prepared, _) = prepared_fixture(PreparedImageStorage::Artifact);
        let frames = with_manager(|manager| {
            complete_decode(manager, invoke.clone(), output);
            match stage {
                0 => {}
                1 => respond(
                    manager,
                    &invoke.call_id,
                    Ok(ArtifactValue::Upload {
                        upload: "prepared-upload".parse().expect("upload"),
                    }),
                ),
                _ => write_prepared(manager, &invoke.call_id, &prepared),
            }
            respond(manager, &invoke.call_id, Err(ArtifactError::Busy));
            assert!(manager.calls.is_empty());
            assert!(manager.requests.is_empty());
        });
        let terminal = terminals(&frames);
        let [Event::ToolErrorReported(error)] = terminal.as_slice() else {
            panic!("one error");
        };
        assert_eq!(error.message, "artifact storage is busy");
        assert_eq!(frames.iter().filter(|frame| matches!(frame,
            HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Begin { .. }))).count(), 1);
    }
}

/// Session retirement keeps unknown allocation cleanup bound to its original
/// session, while transport retirement discards all late-response authority.
#[test]
fn prepared_upload_shutdown_preserves_only_captured_cleanup_authority() {
    for transport_closed in [false, true] {
        let (invoke, output, _, _) = prepared_fixture(PreparedImageStorage::Artifact);
        let frames = with_manager(|manager| {
            complete_decode(manager, invoke.clone(), output);
            let id = request_id(manager, &invoke.call_id);
            if transport_closed {
                manager.shutdown_transport();
            } else {
                manager.shutdown_session();
                manager.bind_session("session-new".parse().expect("new session"));
            }
            manager.handle_result(ArtifactResult {
                request_id: id,
                result: Ok(ArtifactValue::Upload {
                    upload: "late-upload".parse().expect("upload"),
                }),
            });
            assert!(manager.calls.is_empty());
            assert!(manager.cleanup_allocations.is_empty());
        });
        assert!(terminals(&frames).is_empty());
        let aborts = frames
            .iter()
            .filter_map(|frame| match frame {
                HarnessInputMessage::ArtifactRequest(request)
                    if matches!(request.op, ArtifactOp::Abort { .. }) =>
                {
                    Some(request)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(aborts.len(), usize::from(!transport_closed));
        for request in aborts {
            assert_eq!(request.expected_session_id.as_str(), "session-old");
        }
    }
}

/// A stale decoder completion must not remove or upload for a reused call id,
/// regardless of whether its successor is downloading or awaiting decode.
#[test]
fn stale_decoder_completion_cannot_claim_a_successor_invocation() {
    for downloading in [false, true] {
        let (invoke, stale, _, _) = prepared_fixture(PreparedImageStorage::Artifact);
        let (successor, fresh, expected, _) = prepared_fixture(PreparedImageStorage::Inline);
        let frames = with_manager(|manager| {
            let old_token = Arc::new(AtomicBool::new(true));
            let new_token = Arc::new(AtomicBool::new(false));
            if downloading {
                manager.start(successor.clone());
            } else {
                manager.calls.insert(
                    successor.call_id.clone(),
                    ActiveCall::Decoding {
                        invoke: successor.clone(),
                        cancelled: Arc::clone(&new_token),
                    },
                );
            }
            manager.decode_active = true;
            manager
                .completions
                .lock()
                .expect("queue")
                .push_back(DecodeCompletion {
                    call_id: invoke.call_id.clone(),
                    cancelled: old_token,
                    result: Ok(stale),
                });
            manager.drain();
            assert!(manager.calls.contains_key(&successor.call_id));
            assert_eq!(manager.requests.len(), usize::from(downloading));
            if !downloading {
                manager
                    .completions
                    .lock()
                    .expect("queue")
                    .push_back(DecodeCompletion {
                        call_id: successor.call_id.clone(),
                        cancelled: new_token,
                        result: Ok(fresh),
                    });
                manager.drain();
            }
        });
        if downloading {
            assert!(terminals(&frames).is_empty());
        } else {
            let terminal = terminals(&frames);
            let [Event::ToolResultReported(result)] = terminal.as_slice() else {
                panic!("fresh inline result");
            };
            assert_eq!(
                result.provider_content[0]
                    .inline_image()
                    .expect("inline")
                    .data
                    .as_ref(),
                expected
            );
        }
        assert!(!frames.iter().any(|frame| matches!(frame,
            HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Begin { .. }))));
    }
}

/// Duplicate starts cannot replace a live invocation's captured Inline policy.
#[test]
fn duplicate_image_start_preserves_original_policy_and_transfer() {
    let (invoke, _, _, _) = prepared_fixture(PreparedImageStorage::Inline);
    let mut duplicate = invoke.clone();
    duplicate.invocation_policy.prepared_image_storage = PreparedImageStorage::Artifact;
    let frames = with_manager(|manager| {
        manager.start(invoke.clone());
        let original = request_id(manager, &invoke.call_id);
        manager.start(duplicate);
        assert_eq!(request_id(manager, &invoke.call_id), original);
        assert_eq!(
            manager.calls[&invoke.call_id]
                .invoke()
                .invocation_policy
                .prepared_image_storage,
            PreparedImageStorage::Inline
        );
    });
    assert_eq!(frames.iter().filter(|frame| matches!(frame,
        HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Open { .. }))).count(), 1);
    assert!(terminals(&frames).is_empty());
}

/// Uploads and canceled Begins share the same eight-call bound with downloads.
#[test]
fn prepared_upload_and_late_begin_cleanup_share_call_capacity() {
    let frames = with_manager(|manager| {
        for index in 0..MAX_ACTIVE_READS {
            let (mut invoke, output, _, _) = prepared_fixture(PreparedImageStorage::Artifact);
            invoke.call_id = ToolCallId::new(format!("upload-{index}"));
            complete_decode(manager, invoke, output);
        }
        assert_eq!(manager.calls.len(), MAX_ACTIVE_READS);
        let invokes = manager
            .calls
            .values()
            .map(|call| call.invoke().clone())
            .collect::<Vec<_>>();
        for invoke in invokes {
            manager.cancel(&cancellation(&invoke));
        }
        assert_eq!(manager.cleanup_allocations.len(), MAX_ACTIVE_READS);
        let (mut next, _, _, _) = prepared_fixture(PreparedImageStorage::Artifact);
        next.call_id = ToolCallId::new("overloaded");
        manager.start(next.clone());
        assert!(manager.calls.is_empty());
        let id = manager
            .cleanup_allocations
            .keys()
            .next()
            .expect("pending Begin")
            .clone();
        manager.handle_result(ArtifactResult {
            request_id: id,
            result: Ok(ArtifactValue::Upload {
                upload: "late-upload".parse().expect("upload"),
            }),
        });
        next.call_id = ToolCallId::new("replacement");
        manager.start(next);
        assert_eq!(
            manager.calls.len() + manager.cleanup_allocations.len(),
            MAX_ACTIVE_READS
        );
    });
    assert_eq!(
        terminals(&frames)
            .iter()
            .filter(|event| matches!(event, Event::ToolCancelledReported(_)))
            .count(),
        MAX_ACTIVE_READS
    );
    assert_eq!(
        terminals(&frames)
            .iter()
            .filter(|event| matches!(event, Event::ToolErrorReported(_)))
            .count(),
        1
    );
}

/// The prepared transfer keeps the existing sequential one-MiB chunk bound.
#[test]
fn prepared_upload_chunks_without_a_semantic_inline_copy() {
    let (invoke, mut output, _, _) = prepared_fixture(PreparedImageStorage::Artifact);
    let ToolResultContentPart::Image(image) = &mut output.provider_content[0] else {
        panic!("fixture");
    };
    let prepared = vec![7; tau_proto::ARTIFACT_CHUNK_BYTES + 5];
    image.data = Arc::from(prepared.clone());
    let frames = with_manager(|manager| {
        complete_decode(manager, invoke.clone(), output);
        write_prepared(manager, &invoke.call_id, &prepared);
        respond(
            manager,
            &invoke.call_id,
            Ok(ArtifactValue::Descriptor(descriptor(&prepared))),
        );
    });
    let chunks = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::ArtifactRequest(request) => match &request.op {
                ArtifactOp::Write { bytes, .. } => Some(bytes.len()),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(chunks, [tau_proto::ARTIFACT_CHUNK_BYTES, 5]);
    let terminal = terminals(&frames);
    let [Event::ToolResultReported(result)] = terminal.as_slice() else {
        panic!("reference result");
    };
    assert!(result.provider_content[0].is_image_reference());
}

/// Even an invalid internal prepared result cannot exceed the image upload
/// bound.
#[test]
fn oversized_prepared_upload_fails_before_begin() {
    let (invoke, mut output, _, _) = prepared_fixture(PreparedImageStorage::Artifact);
    let ToolResultContentPart::Image(image) = &mut output.provider_content[0] else {
        panic!("fixture");
    };
    image.data = Arc::from(vec![0; crate::read_image::MAX_ENCODED_BYTES + 1]);
    let frames = with_manager(|manager| complete_decode(manager, invoke, output));
    assert!(matches!(
        terminals(&frames).as_slice(),
        [Event::ToolErrorReported(_)]
    ));
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, HarnessInputMessage::ArtifactRequest(_)))
    );
}

/// The real utility loop retains Artifact policy across download, off-loop
/// crop, upload and terminal reporting. Only this synthetic harness selects
/// Artifact; production policy remains Inline until admission and provider
/// resolution exist.
#[cfg(unix)]
#[test]
fn utility_loop_uploads_prepared_bytes_before_reference_terminal() {
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::thread;
    use std::time::Duration;

    let original = source_bytes();
    let (invoke, _, prepared, original_key) = prepared_fixture(PreparedImageStorage::Artifact);
    let expected = descriptor(&prepared);
    let (harness, extension) = UnixStream::pair().expect("socket pair");
    harness
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("bounded fixture");
    let input = extension.try_clone().expect("extension reader");
    let worker = thread::spawn(move || crate::run(input, extension).expect("utility loop"));
    let shutdown = harness.try_clone().expect("shutdown");
    let mut reader =
        tau_proto::HarnessInputReader::new(BufReader::new(harness.try_clone().expect("reader")));
    let mut writer = tau_proto::HarnessOutputWriter::new(harness);
    assert!(matches!(
        reader.read_message().expect("hello"),
        Some(HarnessInputMessage::Hello(_))
    ));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            artifact_limits: Default::default(),
            purpose: Default::default(),
            harness_protocol_version: Some(tau_proto::PROTOCOL_VERSION),
            config: tau_proto::json_to_cbor(&serde_json::json!({"papercut":{"enable":false}})),
            instance_name: "std-utils".parse().expect("instance"),
            tool_prefix: None,
            state_dir: None,
            secrets: Default::default(),
            absent_optional_secrets: Default::default(),
            settings_files: Default::default(),
        }))
        .expect("configure");
    writer.flush().expect("configure flush");
    loop {
        let frame = reader
            .read_message()
            .expect("startup")
            .expect("connected during startup");
        if matches!(frame, HarnessInputMessage::Ready(_)) {
            break;
        }
    }
    for event in [
        Event::SessionStarted(tau_proto::SessionStarted {
            session_id: "upload-session".parse().expect("session"),
            reason: tau_proto::SessionStartReason::Initial,
        }),
        Event::ToolStarted(invoke.clone()),
    ] {
        writer
            .write_message(&HarnessOutputMessage::deliver_live(
                tau_proto::UnixMicros::new(1),
                event,
            ))
            .expect("delivery");
    }
    writer.flush().expect("delivery flush");
    let mut uploaded = Vec::new();
    let mut finalized = false;
    let mut closed = false;
    loop {
        match reader
            .read_message()
            .expect("utility output")
            .expect("connected")
        {
            HarnessInputMessage::ArtifactRequest(request) => {
                assert_eq!(request.expected_session_id.as_str(), "upload-session");
                let value = match request.op {
                    ArtifactOp::Open { key } => {
                        assert_eq!(key, original_key);
                        ArtifactValue::Opened {
                            read: "original-read".parse().expect("read"),
                            descriptor: descriptor(&original),
                        }
                    }
                    ArtifactOp::Read { offset, .. } => {
                        assert_eq!(offset, 0);
                        ArtifactValue::Chunk {
                            offset,
                            bytes: original.clone(),
                            eof: true,
                        }
                    }
                    ArtifactOp::Close { .. } => {
                        closed = true;
                        ArtifactValue::Done
                    }
                    ArtifactOp::Begin { size } => {
                        assert!(closed, "source transfer releases its slot before upload");
                        assert_eq!(size, expected.size);
                        ArtifactValue::Upload {
                            upload: "prepared-upload".parse().expect("upload"),
                        }
                    }
                    ArtifactOp::Write { offset, bytes, .. } => {
                        assert_eq!(offset, uploaded.len() as u64);
                        uploaded.extend_from_slice(&bytes);
                        ArtifactValue::Written {
                            next_offset: uploaded.len() as u64,
                        }
                    }
                    ArtifactOp::Finalize { .. } => {
                        assert_eq!(uploaded, prepared);
                        finalized = true;
                        ArtifactValue::Descriptor(expected.clone())
                    }
                    _ => panic!("unexpected transfer"),
                };
                writer
                    .write_message(&HarnessOutputMessage::ArtifactResult(Box::new(
                        ArtifactResult {
                            request_id: request.request_id,
                            result: Ok(value),
                        },
                    )))
                    .expect("response");
                writer.flush().expect("response flush");
            }
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::ToolResultReported(result) => {
                    assert!(finalized);
                    assert_eq!(result.call_id, invoke.call_id);
                    let [ToolResultContentPart::ImageReference(image)] =
                        result.provider_content.as_slice()
                    else {
                        panic!("no semantic inline payload");
                    };
                    assert_eq!(image.blob, expected);
                    break;
                }
                Event::ToolErrorReported(error) => panic!("unexpected error: {}", error.message),
                _ => {}
            },
            _ => {}
        }
    }
    shutdown.shutdown(Shutdown::Write).expect("close input");
    worker.join().expect("utility thread");
}
