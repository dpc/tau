use std::io::{BufReader, Cursor};
use std::time::Duration;

use tau_client::{ExtensionBuilder, TauExtension, TauExtensionRunner};
use tau_proto::{ArtifactDescriptor, ArtifactError, HarnessInputMessage, HarnessOutputMessage};

use super::*;
use crate::tests::SharedTraceWriter;

/// Minimal transport owner: focused state tests never invoke a network backend.
struct TestExtension;

impl TauExtension for TestExtension {
    type State = ();
    fn name(&self) -> &'static str {
        "image-test"
    }
    fn register(self, _builder: &mut ExtensionBuilder<Self::State>) {}
}

/// Flushes real SDK output, keeping tests independent of private handle
/// internals.
fn with_handle(f: impl FnOnce(&ClientHandle)) -> Vec<HarnessInputMessage> {
    with_transport(|handle, _| f(handle))
}

fn with_transport(
    f: impl FnOnce(&ClientHandle, tau_client::ManualRuntimeWaker),
) -> Vec<HarnessInputMessage> {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            purpose: Default::default(),
            config: CborValue::Map(Vec::new()),
            instance_name: "image-test".parse().expect("instance"),
            tool_prefix: None,
            state_dir: None,
            secrets: Default::default(),
            settings_files: Default::default(),
        }))
        .expect("configure");
    writer.flush().expect("flush");
    drop(writer);
    let output = SharedTraceWriter::default();
    let runtime = TauExtensionRunner::new(TestExtension)
        .start_manual_loop(Cursor::new(input), output.clone(), ())
        .expect("runtime");
    f(&runtime.handle(), runtime.waker());
    runtime.finish().expect("finish");
    let bytes = output.bytes();
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(bytes.as_slice()));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("decode") {
        frames.push(frame);
    }
    frames
}

/// Availability is mandatory and credentials come only from the selected
/// declaration, even when another account exists.
#[test]
fn image_available_starts_exactly_one_selected_account_worker() {
    let frames = with_transport(|handle, waker| {
        let mut runtime = crate::tests::observation_test_runtime();
        runtime.images = images();
        runtime.worker_waker = Some(waker);
        runtime.load_prompt_profiles = |selected| {
            assert_eq!(selected, Some(&ProviderName::new("selected")));
            let mut profiles = BuiltinProviderProfiles::default();
            let mut profile = crate::ChatGptProfile::default();
            profile.auth.access_token = "selected-token".into();
            profile.auth.account_id = Some("selected-account".into());
            profiles.providers.insert(
                ProviderName::new("selected"),
                BuiltinProviderProfile::Chatgpt(profile),
            );
            profiles
        };
        runtime.images.executor = |_, prompt, id, _, cancelled| {
            assert_eq!(prompt, "a transparent leaf");
            assert_eq!(id, "image-call");
            assert!(!cancelled.is_cancelled());
            Err(GenerationError::Quota)
        };
        runtime.start_image_call(started(), handle).expect("start");
        assert!(runtime.worker_rx.try_recv().is_err());
        let request_id = runtime
            .images
            .requests
            .keys()
            .next()
            .expect("request")
            .clone();
        let result = ArtifactResult {
            request_id,
            result: Ok(ArtifactValue::Done),
        };
        runtime
            .handle_image_artifact_result(result.clone(), handle)
            .expect("available");
        runtime
            .handle_image_artifact_result(result, handle)
            .expect("duplicate ignored");
        let message = runtime
            .worker_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker");
        let WorkerMessage::ImageGenerated { call_id, result } = message else {
            panic!("unexpected worker message");
        };
        assert_eq!(result, Err(GenerationError::Quota));
        runtime
            .images
            .generated(call_id, result, handle)
            .expect("terminal");
        assert!(runtime.images.calls.is_empty());
        assert!(runtime.worker_rx.try_recv().is_err());
    });
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(frame, HarnessInputMessage::ArtifactRequest(_)))
            .count(),
        1
    );
    assert_eq!(frames.iter().filter(|frame| matches!(frame, HarnessInputMessage::Emit(emit) if matches!(*emit.event, tau_proto::Event::ToolErrorReported(_)))).count(), 1);
}

fn started() -> ToolStarted {
    ToolStarted {
        call_id: "image-call".into(),
        tool_name: ToolName::new("image_internal"),
        arguments: CborValue::Map(vec![(
            CborValue::Text("prompt".into()),
            CborValue::Text("a transparent leaf".into()),
        )]),
        agent_id: "agent".parse().expect("agent"),
        originator: Default::default(),
        invocation_policy: Default::default(),
    }
}

fn images() -> ImageTools {
    ImageTools {
        declarations: HashMap::from([(
            ToolName::new("image_internal"),
            ProviderName::new("selected"),
        )]),
        session: Some("session".parse().expect("session")),
        ..Default::default()
    }
}

fn call(phase: Phase) -> ImageCall {
    ImageCall {
        started: started(),
        session: "session".parse().expect("session"),
        provider: ProviderName::new("selected"),
        prompt: "leaf".into(),
        cancel: Arc::new(GenerationCancellation::default()),
        deadline: None,
        phase,
    }
}

/// Default-off declarations are pure, account-specific and have one flat alias.
#[test]
fn image_declarations_are_opt_in_and_prompt_only() {
    let mut profiles = BuiltinProviderProfiles::default();
    for (name, enabled, lite) in [("a", true, false), ("b", true, true), ("c", false, false)] {
        profiles.providers.insert(
            ProviderName::new(name),
            BuiltinProviderProfile::Chatgpt(crate::ChatGptProfile {
                image_generation: enabled,
                responses_lite_compatibility: lite,
                ..Default::default()
            }),
        );
    }
    let tools = declarations(&profiles);
    assert_eq!(tools.len(), 2);
    assert_ne!(tools[0].tool.name, tools[1].tool.name);
    for tool in tools {
        assert!(tool.tool.provider_scope.is_some());
        assert_eq!(
            tool.tool.model_visible_name,
            Some(ToolName::new("generate_image"))
        );
        assert!(tool.tool.tags.is_empty());
        assert!(tool.tool_group.is_none());
        let schema = tool.tool.parameters.expect("schema");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"].as_object().expect("properties").len(),
            1
        );
    }
    assert!(!crate::ChatGptProfile::default().image_generation);
    let mut args = started().arguments;
    assert!(parse_prompt(&args).is_some());
    let CborValue::Map(entries) = &mut args else {
        unreachable!()
    };
    entries.push((
        CborValue::Text("account".into()),
        CborValue::Text("other".into()),
    ));
    assert!(parse_prompt(&args).is_none());
    for text in [
        " ".to_owned(),
        "x".repeat(image_generation::MAX_PROMPT_BYTES + 1),
    ] {
        assert!(
            parse_prompt(&CborValue::Map(vec![(
                CborValue::Text("prompt".into()),
                CborValue::Text(text)
            )]))
            .is_none()
        );
    }
}

/// Storage denial terminalizes before credentials or generation can be loaded.
#[test]
fn image_availability_failure_does_not_generate() {
    let frames = with_handle(|handle| {
        let mut runtime = crate::tests::observation_test_runtime();
        runtime.images = images();
        runtime.images.start(started(), handle).expect("start");
        assert!(matches!(
            runtime.images.calls.values().next().expect("call").phase,
            Phase::Availability
        ));
        let request_id = runtime
            .images
            .requests
            .keys()
            .next()
            .expect("request")
            .clone();
        runtime
            .handle_image_artifact_result(
                ArtifactResult {
                    request_id,
                    result: Err(ArtifactError::Permission),
                },
                handle,
            )
            .expect("denial");
        assert!(runtime.images.calls.is_empty());
        assert!(runtime.worker_rx.try_recv().is_err());
    });
    assert_eq!(frames.iter().filter(|frame| matches!(frame, HarnessInputMessage::ArtifactRequest(request) if request.op == ArtifactOp::Available)).count(), 1);
    assert_eq!(frames.iter().filter(|frame| matches!(frame, HarnessInputMessage::Emit(emit) if matches!(*emit.event, tau_proto::Event::ToolErrorReported(_)))).count(), 1);
}

/// Upload copies every original byte and reports only a verified hash
/// descriptor.
#[test]
fn image_original_upload_returns_key_without_path_or_inline_bytes() {
    let original = vec![0x9b; tau_proto::ARTIFACT_CHUNK_BYTES + 7];
    let descriptor = ArtifactDescriptor::new(
        format!("blake3:{}", blake3::hash(&original).to_hex())
            .parse()
            .expect("key"),
        original.len() as u64,
    )
    .expect("descriptor");
    let frames = with_handle(|handle| {
        let mut runtime = crate::tests::observation_test_runtime();
        runtime
            .images
            .calls
            .insert(started().call_id, call(Phase::Generating));
        runtime
            .images
            .generated(started().call_id, Ok(original.clone()), handle)
            .expect("generated");
        runtime
            .images
            .generated(started().call_id, Err(GenerationError::Transport), handle)
            .expect("duplicate worker ignored without losing upload");
        for value in [
            ArtifactValue::Upload {
                upload: "upload".parse().expect("upload"),
            },
            ArtifactValue::Written {
                next_offset: tau_proto::ARTIFACT_CHUNK_BYTES as u64,
            },
            ArtifactValue::Written {
                next_offset: original.len() as u64,
            },
            ArtifactValue::Descriptor(descriptor.clone()),
        ] {
            let request_id = runtime
                .images
                .requests
                .keys()
                .next()
                .expect("request")
                .clone();
            runtime
                .handle_image_artifact_result(
                    ArtifactResult {
                        request_id,
                        result: Ok(value),
                    },
                    handle,
                )
                .expect("advance");
        }
        assert!(runtime.images.calls.is_empty());
    });
    let written: Vec<u8> = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::ArtifactRequest(request) => match &request.op {
                ArtifactOp::Write { bytes, .. } => Some(bytes.as_slice()),
                _ => None,
            },
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    assert_eq!(written, original);
    let results: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                tau_proto::Event::ToolResultReported(result) => Some(result),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].provider_content.is_empty());
    let CborValue::Map(fields) = &results[0].result else {
        panic!("result map");
    };
    assert_eq!(fields.len(), 3);
    assert!(fields.contains(&(
        CborValue::Text("key".into()),
        CborValue::Text(descriptor.key.to_string())
    )));
    assert!(
        !fields
            .iter()
            .any(|(key, _)| key == &CborValue::Text("path".into()))
    );
}

/// Cancellation at every phase suppresses late replies without deleting
/// objects.
#[test]
fn image_cancellation_and_timeout_invalidate_late_publication() {
    for timeout in [false, true] {
        let mut finalizing = ArtifactUpload::new(b"original".to_vec()).expect("upload");
        finalizing
            .accept(ArtifactValue::Upload {
                upload: "finalizing".parse().expect("upload"),
            })
            .expect("begin");
        finalizing
            .accept(ArtifactValue::Written { next_offset: 8 })
            .expect("write");
        for phase in [
            Phase::Availability,
            Phase::Generating,
            Phase::Uploading(ArtifactUpload::new(b"original".to_vec()).expect("upload")),
            Phase::Uploading(finalizing),
        ] {
            let frames = with_handle(|handle| {
                let mut runtime = crate::tests::observation_test_runtime();
                let call = call(phase);
                let flag = Arc::clone(&call.cancel);
                runtime.images.calls.insert(started().call_id, call);
                let request_id: ArtifactRequestId = "pending".parse().expect("request");
                runtime
                    .images
                    .requests
                    .insert(request_id.clone(), started().call_id);
                if timeout {
                    runtime
                        .images
                        .timeout(&started().call_id, handle)
                        .expect("timeout");
                } else {
                    runtime
                        .images
                        .cancel(&started().call_id, handle)
                        .expect("cancel");
                }
                assert!(flag.is_cancelled());
                runtime
                    .images
                    .generated(started().call_id, Ok(b"late original".to_vec()), handle)
                    .expect("late worker");
                runtime
                    .handle_image_artifact_result(
                        ArtifactResult {
                            request_id,
                            result: Ok(ArtifactValue::Done),
                        },
                        handle,
                    )
                    .expect("late RPC");
                assert!(runtime.images.calls.is_empty());
            });
            assert_eq!(frames.iter().filter(|frame| matches!(frame, HarnessInputMessage::Emit(emit) if matches!(*emit.event, tau_proto::Event::ToolErrorReported(_) | tau_proto::Event::ToolCancelledReported(_)))).count(), 1);
            assert!(!frames.iter().any(|frame| matches!(frame, HarnessInputMessage::Emit(emit) if matches!(*emit.event, tau_proto::Event::ToolResultReported(_)))));
        }
    }
}

/// Storage faults after the paid effect never schedule another worker.
#[test]
fn image_upload_failure_aborts_without_regeneration() {
    let frames = with_handle(|handle| {
        let mut runtime = crate::tests::observation_test_runtime();
        runtime
            .images
            .calls
            .insert(started().call_id, call(Phase::Generating));
        runtime
            .images
            .generated(started().call_id, Ok(b"original".to_vec()), handle)
            .expect("generated");
        for value in [
            Ok(ArtifactValue::Upload {
                upload: "failed-upload".parse().expect("upload"),
            }),
            Err(ArtifactError::Permission),
        ] {
            let request_id = runtime
                .images
                .requests
                .keys()
                .next()
                .expect("request")
                .clone();
            runtime
                .handle_image_artifact_result(
                    ArtifactResult {
                        request_id,
                        result: value,
                    },
                    handle,
                )
                .expect("reply");
        }
        assert!(runtime.images.calls.is_empty());
        assert!(runtime.images.requests.is_empty());
        assert!(runtime.worker_rx.try_recv().is_err());
    });
    assert_eq!(frames.iter().filter(|frame| matches!(frame, HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Abort { .. }))).count(), 1);
    assert_eq!(frames.iter().filter(|frame| matches!(frame, HarnessInputMessage::Emit(emit) if matches!(*emit.event, tau_proto::Event::ToolErrorReported(_)))).count(), 1);
}
