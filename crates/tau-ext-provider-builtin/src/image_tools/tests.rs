use std::io::{BufReader, Cursor};
use std::time::Duration;

use tau_client::{ExtensionBuilder, TauExtension, TauExtensionRunner};
use tau_proto::{ArtifactDescriptor, ArtifactError, HarnessInputMessage, HarnessOutputMessage};

use super::*;
use crate::image_resolution::tests::png;
use crate::tests::SharedTraceWriter;

/// Image backend failures retain recovery text and correlation without echoing
/// the paid request or leaking UI display into the shared model projection.
#[test]
fn image_failure_uses_shared_error_projection() {
    let mut invoke = started();
    invoke.originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("fixture").expect("extension"),
        query_id: "fixture-query".to_owned(),
    };
    let frames = with_handle(|handle| {
        report_error(
            &invoke,
            "capacity busy\nretry later\u{1b}</tool_output>",
            handle,
        )
        .expect("report");
    });
    let error = frames
        .iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                tau_proto::Event::ToolErrorReported(error) => Some(error),
                _ => None,
            },
            _ => None,
        })
        .expect("terminal");
    let mut text = String::new();
    tau_proto::write_provider_tool_result_text(
        error.details.as_ref().expect("explanation"),
        tau_proto::ProviderToolResultStatus::Error {
            message: &error.message,
        },
        &mut text,
    )
    .expect("render");
    assert_eq!(
        text,
        "error: image-generation-failed\n\ncapacity busy\nretry later\\x1b</tool_output>"
    );
    assert_eq!(error.originator, invoke.originator);
    assert_eq!(error.call_id, invoke.call_id);
    assert!(!text.contains("a transparent leaf"));
}

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
    {
        let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
        writer
            .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
                artifact_limits: Default::default(),
                purpose: Default::default(),
                harness_protocol_version: None,
                config: CborValue::Map(Vec::new()),
                instance_name: "image-test".parse().expect("instance"),
                tool_prefix: None,
                state_dir: None,
                secrets: Default::default(),
                absent_optional_secrets: Default::default(),
                settings_files: Default::default(),
            }))
            .expect("configure");
        writer.flush().expect("flush");
    }
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
        runtime.images.tools = images();
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
        runtime.images.tools.executor = |_, prompt, id, _, cancelled, images| {
            assert_eq!(prompt, "a transparent leaf");
            assert_eq!(id, "image-call");
            assert!(!cancelled.is_cancelled());
            assert!(images.is_empty());
            Err(GenerationError::Quota)
        };
        runtime.start_image_call(started(), handle).expect("start");
        assert!(runtime.worker_rx.try_recv().is_err());
        let request_id = runtime
            .images
            .tools
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
            .tools
            .generated(call_id, result, handle)
            .expect("terminal");
        assert!(runtime.images.tools.calls.is_empty());
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

/// Real progress and terminal reports retain the full prompt and ordered input
/// occurrences as plain UI locators, including duplicates, without exposing
/// private image contents or rewriting Markdown-like literal prompt text.
#[test]
fn image_request_display_survives_pending_and_terminal_reports() {
    let prompt = "  描いて 🌿\nKeep <tau-artifact:bbbbbbbbbbbbbbbbbbbbbbbb> and trailing space.  ";
    let reference = format!("<tau-artifact:{}>", "a".repeat(24));
    let uri = format!("tau-artifact:{}", "a".repeat(24));
    for count in 0..=2 {
        for fail in [false, true] {
            let frames = with_handle(|handle| {
                let mut tools = images();
                let mut started = started();
                started.arguments = CborValue::Map(vec![
                    (
                        CborValue::Text("prompt".into()),
                        CborValue::Text(prompt.into()),
                    ),
                    (
                        CborValue::Text("attachments".into()),
                        CborValue::Array(vec![CborValue::Text(reference.clone()); count]),
                    ),
                ]);
                tools.start(started.clone(), handle).expect("start");
                if fail {
                    tools.timeout(&started.call_id, handle).expect("timeout");
                } else {
                    tools.cancel(&started.call_id, handle).expect("cancel");
                }
            });
            let displays: Vec<_> = frames
                .iter()
                .filter_map(|frame| {
                    let HarnessInputMessage::Emit(emit) = frame else {
                        return None;
                    };
                    match emit.event.as_ref() {
                        tau_proto::Event::ToolProgressReported(event) => event.display.as_ref(),
                        tau_proto::Event::ToolErrorReported(event) => event.display.as_ref(),
                        tau_proto::Event::ToolCancelledReported(event) => event.display.as_ref(),
                        _ => None,
                    }
                })
                .collect();
            assert_eq!(displays.len(), 2);
            assert_eq!(displays[0].status, tau_proto::ToolUseStatus::InProgress);
            assert_eq!(displays[0].status_text, "pending");
            assert_eq!(
                displays[1].status,
                if fail {
                    tau_proto::ToolUseStatus::Error
                } else {
                    tau_proto::ToolUseStatus::Warning
                }
            );
            for display in displays {
                assert!(display.args.is_empty());
                assert_eq!(display.stats.bytes, Some(prompt.len() as u64));
                assert_eq!(display.progress_counters.len(), usize::from(count != 0));
                if count != 0 {
                    assert_eq!(
                        display.progress_counters[0],
                        tau_proto::ProgressCounter {
                            label: Some("att".into()),
                            unit: tau_proto::ProgressUnit::Count,
                            complete: Some(count as u64),
                            total: None,
                        }
                    );
                }
                let Some(tau_proto::ToolUsePayload::Text { text }) = &display.payload else {
                    panic!("full request payload");
                };
                assert!(text.starts_with(&format!("Prompt:\n{prompt}")));
                assert_eq!(text.matches(&uri).count(), count);
                assert!(!text.contains(&reference));
            }
        }
    }
}

/// Malformed requests must not echo arbitrary unknown fields or inline image
/// blobs into the display while reporting validation failure.
#[test]
fn image_invalid_request_has_no_display_payload() {
    let frames = with_handle(|handle| {
        let mut tools = images();
        let mut started = started();
        started.arguments = CborValue::Text("private malformed request".into());
        tools.start(started, handle).expect("validation error");
    });
    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), tau_proto::Event::ToolErrorReported(error)
                if error.display.is_none())
    )));
}

fn call(phase: Phase) -> ImageCall {
    ImageCall {
        started: started(),
        session: "session".parse().expect("session"),
        provider: ProviderName::new("selected"),
        prompt: "leaf".into(),
        attachments: Vec::new(),
        cancel: Arc::new(GenerationCancellation::default()),
        deadline: None,
        phase,
    }
}

/// Default-on profile capability declares a pure account-specific tool while
/// preserving the default-off role surface and explicit profile opt-out.
#[test]
fn image_declarations_default_on_but_remain_role_authorized() {
    let mut profiles = BuiltinProviderProfiles::default();
    for (name, enabled, lite) in [("a", true, false), ("b", true, true), ("c", false, false)] {
        profiles.providers.insert(
            ProviderName::new(name),
            BuiltinProviderProfile::Chatgpt(crate::ChatGptProfile {
                image_generation: enabled,
                responses: tau_config::chatgpt_responses_settings::ChatgptResponsesSettings {
                    responses_lite_compatibility: lite,
                },
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
        assert!(!tool.tool.enabled_by_default);
        assert!(tool.tool.tags.is_empty());
        assert!(tool.tool_group.is_none());
        let schema = tool.tool.parameters.expect("schema");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["attachments"]["items"]["pattern"],
            tau_proto::ARTIFACT_ARGUMENT_PATTERN
        );
        assert_eq!(
            schema["properties"].as_object().expect("properties").len(),
            2
        );
    }
    assert!(crate::ChatGptProfile::default().image_generation);
    let mut args = started().arguments;
    assert!(parse_arguments(&args).is_some());
    let CborValue::Map(entries) = &mut args else {
        unreachable!()
    };
    entries.push((
        CborValue::Text("account".into()),
        CborValue::Text("other".into()),
    ));
    assert!(parse_arguments(&args).is_none());
    for text in [
        " ".to_owned(),
        "x".repeat(image_generation::MAX_PROMPT_BYTES + 1),
    ] {
        assert!(
            parse_arguments(&CborValue::Map(vec![(
                CborValue::Text("prompt".into()),
                CborValue::Text(text)
            )]))
            .is_none()
        );
    }
}

/// Adds only explicit ordered originals, never history selection or paths.
fn attached(mut call: ToolStarted, originals: &[Vec<u8>]) -> ToolStarted {
    let CborValue::Map(entries) = &mut call.arguments else {
        unreachable!("arguments")
    };
    entries.push((
        CborValue::Text("attachments".into()),
        CborValue::Array(
            originals
                .iter()
                .map(|bytes| CborValue::Text(tau_proto::artifact_reference(&descriptor(bytes).key)))
                .collect(),
        ),
    ));
    call
}

/// Constructs exact content identity and size without a store or user data.
fn descriptor(bytes: &[u8]) -> ArtifactDescriptor {
    ArtifactDescriptor::from_bytes(
        bytes,
        tau_proto::ArtifactIdentity::metadata(None, Some("image/png".into())).unwrap(),
    )
    .expect("descriptor")
}

/// Supplies one correlated storage result through the production main loop.
fn reply(
    runtime: &mut ProviderRuntime<fn(Option<&ProviderName>) -> BuiltinProviderProfiles>,
    value: Result<ArtifactValue, ArtifactError>,
    handle: &ClientHandle,
) {
    let request_id = runtime
        .images
        .tools
        .requests
        .keys()
        .next()
        .expect("outstanding")
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

/// Configures synthetic selected-account authority without loading real state.
fn input_runtime(
    waker: tau_client::ManualRuntimeWaker,
) -> ProviderRuntime<fn(Option<&ProviderName>) -> BuiltinProviderProfiles> {
    let mut runtime = crate::tests::observation_test_runtime();
    runtime.images.tools = images();
    runtime.worker_waker = Some(waker);
    runtime.load_prompt_profiles = |selected| {
        assert_eq!(selected, Some(&ProviderName::new("selected")));
        let mut profiles = BuiltinProviderProfiles::default();
        let mut profile = crate::ChatGptProfile::default();
        profile.auth.access_token = "synthetic-selected".into();
        profiles.providers.insert(
            ProviderName::new("selected"),
            BuiltinProviderProfile::Chatgpt(profile),
        );
        profiles
    };
    runtime
}

/// Creates a small valid original without filesystem access or provider
/// traffic.
fn input_png() -> Vec<u8> {
    png()
}

/// The strict runtime parser matches the additive schema and does not broaden
/// artifact references into URLs, files, upload IDs or arbitrary objects.
#[test]
fn image_attachment_argument_contract() {
    let base = started();
    let key = descriptor(b"synthetic").key;
    let reference = tau_proto::artifact_reference(&key);
    let legacy: tau_proto::ArtifactKey = blake3::hash(b"synthetic").to_hex()[..24]
        .to_owned()
        .parse()
        .expect("legacy");
    for (values, expected) in [
        (Vec::new(), key.clone()),
        (vec![CborValue::Text(reference.clone())], key.clone()),
        (
            vec![CborValue::Text(tau_proto::artifact_uri(&key))],
            key.clone(),
        ),
        (vec![CborValue::Text(key.to_string()); 5], key.clone()),
        (vec![CborValue::Text(legacy.to_string())], legacy.clone()),
        (
            vec![CborValue::Text(format!("<tau-artifact:{legacy}>"))],
            legacy,
        ),
    ] {
        let mut args = base.arguments.clone();
        let CborValue::Map(entries) = &mut args else {
            unreachable!()
        };
        entries.push((
            CborValue::Text("attachments".into()),
            CborValue::Array(values.clone()),
        ));
        let (_, parsed) = parse_arguments(&args).expect("valid attachments");
        assert_eq!(parsed, vec![expected; values.len()]);
    }
    for invalid in [
        CborValue::Null,
        CborValue::Text(reference.clone()),
        CborValue::Array(vec![CborValue::Null]),
        CborValue::Array(vec![CborValue::Text(reference); 6]),
        CborValue::Array(vec![CborValue::Text(
            "https://example.invalid/image.png".into(),
        )]),
        CborValue::Array(vec![CborValue::Text("/tmp/image.png".into())]),
        CborValue::Array(vec![CborValue::Text("data:image/png;base64,AA==".into())]),
        CborValue::Array(vec![CborValue::Text("file-123".into())]),
        CborValue::Array(vec![CborValue::Text("blake3:bad".into())]),
    ] {
        let mut args = base.arguments.clone();
        let CborValue::Map(entries) = &mut args else {
            unreachable!()
        };
        entries.push((CborValue::Text("attachments".into()), invalid));
        assert!(parse_arguments(&args).is_none());
    }
}

/// Every occurrence downloads its original and acknowledges Close before the
/// one selected-account worker can begin, including repeated references.
#[test]
fn image_edits_resolve_ordered_originals_before_one_worker() {
    let first = input_png();
    let mut second = first.clone();
    // Trailing private metadata remains part of the unchanged original.
    second.extend_from_slice(b"synthetic-original-metadata");
    let originals = vec![first.clone(), second, first];
    let frames = with_transport(|handle, waker| {
        let mut runtime = input_runtime(waker);
        runtime.images.tools.executor = |_, _, _, _, _, inputs| {
            assert_eq!(inputs.len(), 3);
            Err(GenerationError::Quota)
        };
        runtime
            .start_image_call(attached(started(), &originals), handle)
            .expect("start");
        reply(&mut runtime, Ok(ArtifactValue::Done), handle);
        for (index, bytes) in originals.iter().enumerate() {
            reply(
                &mut runtime,
                Ok(ArtifactValue::Opened {
                    read: format!("read-{index}").parse().expect("read"),
                    descriptor: descriptor(bytes),
                }),
                handle,
            );
            reply(
                &mut runtime,
                Ok(ArtifactValue::Chunk {
                    offset: 0,
                    bytes: bytes.clone(),
                    eof: true,
                }),
                handle,
            );
            assert!(runtime.worker_rx.try_recv().is_err(), "must await Close");
            reply(&mut runtime, Ok(ArtifactValue::Done), handle);
        }
        let WorkerMessage::ImageGenerated { call_id, result } = runtime
            .worker_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker")
        else {
            panic!("image result")
        };
        assert_eq!(result, Err(GenerationError::Quota));
        runtime
            .images
            .tools
            .generated(call_id, result, handle)
            .expect("terminal");
        assert!(runtime.images.tools.calls.is_empty());
        assert!(runtime.images.tools.workers.is_empty());
    });
    let ops: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::ArtifactRequest(request) => Some(&request.op),
            _ => None,
        })
        .collect();
    assert_eq!(ops.len(), 10);
    assert_eq!(ops[0], &ArtifactOp::Available);
    for (index, bytes) in originals.iter().enumerate() {
        assert_eq!(
            ops[1 + index * 3],
            &ArtifactOp::Open {
                key: descriptor(bytes).key
            }
        );
        assert!(matches!(ops[3 + index * 3], ArtifactOp::Close { .. }));
    }
}

/// Broken Artifact replies fail exactly once, clean up allocated reads and
/// never invoke the paid executor, even when input bytes look like an image.
#[test]
fn image_input_failures_terminalize_before_paid_work() {
    #[derive(Clone, Copy)]
    enum Fault {
        UnavailableStorage,
        MissingOriginal,
        WrongKey,
        BadOffset,
        BadDigest,
        WrongEof,
        OversizeFrame,
    }
    for fault in [
        Fault::UnavailableStorage,
        Fault::MissingOriginal,
        Fault::WrongKey,
        Fault::BadOffset,
        Fault::BadDigest,
        Fault::WrongEof,
        Fault::OversizeFrame,
    ] {
        let bytes = input_png();
        let frames = with_transport(|handle, waker| {
            let mut runtime = input_runtime(waker);
            runtime.images.tools.executor = |_, _, _, _, _, _| panic!("no paid work");
            runtime
                .start_image_call(attached(started(), &[bytes.clone()]), handle)
                .expect("start");
            if matches!(fault, Fault::UnavailableStorage) {
                reply(&mut runtime, Err(ArtifactError::Unavailable), handle);
            } else {
                reply(&mut runtime, Ok(ArtifactValue::Done), handle);
                if matches!(fault, Fault::MissingOriginal) {
                    reply(&mut runtime, Err(ArtifactError::Unavailable), handle);
                } else {
                    reply(
                        &mut runtime,
                        Ok(ArtifactValue::Opened {
                            read: "input-read".parse().expect("read"),
                            descriptor: if matches!(fault, Fault::WrongKey) {
                                descriptor(b"wrong identity")
                            } else {
                                descriptor(&bytes)
                            },
                        }),
                        handle,
                    );
                    if !matches!(fault, Fault::WrongKey) {
                        reply(
                            &mut runtime,
                            Ok(ArtifactValue::Chunk {
                                offset: if matches!(fault, Fault::BadOffset) {
                                    1
                                } else {
                                    0
                                },
                                bytes: if matches!(fault, Fault::BadDigest) {
                                    vec![0; bytes.len()]
                                } else if matches!(fault, Fault::OversizeFrame) {
                                    vec![0; tau_proto::ARTIFACT_CHUNK_BYTES + 1]
                                } else {
                                    bytes.clone()
                                },
                                eof: !matches!(fault, Fault::WrongEof),
                            }),
                            handle,
                        );
                    }
                }
            }
            assert!(runtime.images.tools.calls.is_empty());
            assert!(runtime.images.tools.opens.is_empty());
            assert!(runtime.worker_rx.try_recv().is_err());
        });
        assert_eq!(frames.iter().filter(|frame| matches!(frame, HarnessInputMessage::Emit(emit) if matches!(*emit.event, tau_proto::Event::ToolErrorReported(_)))).count(), 1);
        if !matches!(fault, Fault::UnavailableStorage | Fault::MissingOriginal) {
            assert!(frames.iter().any(|frame| matches!(frame, HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Close { .. }))));
        }
    }
}

/// Missing/invalid media is rejected by the worker before the executor seam;
/// Artifact digest equality alone never establishes media validity.
#[test]
fn image_invalid_original_never_reaches_executor() {
    with_transport(|handle, waker| {
        let mut runtime = input_runtime(waker);
        runtime.images.tools.executor =
            |_, _, _, _, _, _| panic!("invalid original reached paid work");
        let bytes = b"not image data".to_vec();
        runtime
            .start_image_call(attached(started(), &[bytes.clone()]), handle)
            .expect("start");
        for value in [
            ArtifactValue::Done,
            ArtifactValue::Opened {
                read: "read".parse().expect("read"),
                descriptor: descriptor(&bytes),
            },
            ArtifactValue::Chunk {
                offset: 0,
                bytes,
                eof: true,
            },
            ArtifactValue::Done,
        ] {
            reply(&mut runtime, Ok(value), handle);
        }
        let WorkerMessage::ImageGenerated { call_id, result } = runtime
            .worker_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker")
        else {
            panic!("result")
        };
        assert_eq!(result, Err(GenerationError::InvalidInput));
        runtime
            .images
            .tools
            .generated(call_id, result, handle)
            .expect("terminal");
    });
}

/// Cancellation and timeout at every transfer phase revoke dispatch authority;
/// late Open allocations still close without reviving the canceled call.
#[test]
fn image_input_cancel_and_timeout_revoke_late_replies() {
    for phase in 0..3 {
        for timeout in [false, true] {
            let bytes = input_png();
            let frames = with_transport(|handle, waker| {
                let mut runtime = input_runtime(waker);
                runtime.images.tools.executor = |_, _, _, _, _, _| panic!("canceled call executed");
                runtime
                    .start_image_call(attached(started(), &[bytes.clone()]), handle)
                    .expect("start");
                reply(&mut runtime, Ok(ArtifactValue::Done), handle);
                let opened = ArtifactValue::Opened {
                    read: "read".parse().expect("read"),
                    descriptor: descriptor(&bytes),
                };
                if 1 <= phase {
                    reply(&mut runtime, Ok(opened.clone()), handle);
                }
                if 2 <= phase {
                    reply(
                        &mut runtime,
                        Ok(ArtifactValue::Chunk {
                            offset: 0,
                            bytes,
                            eof: true,
                        }),
                        handle,
                    );
                }
                let request_id = runtime
                    .images
                    .tools
                    .requests
                    .keys()
                    .next()
                    .expect("pending")
                    .clone();
                if timeout {
                    runtime
                        .images
                        .tools
                        .timeout(&started().call_id, handle)
                        .expect("timeout");
                } else {
                    runtime
                        .images
                        .tools
                        .cancel(&started().call_id, handle)
                        .expect("cancel");
                }
                runtime
                    .handle_image_artifact_result(
                        ArtifactResult {
                            request_id,
                            result: Ok(if phase == 0 {
                                opened
                            } else {
                                ArtifactValue::Done
                            }),
                        },
                        handle,
                    )
                    .expect("late");
                assert!(runtime.images.tools.calls.is_empty());
                assert!(runtime.images.tools.opens.is_empty());
                assert!(runtime.worker_rx.try_recv().is_err());
            });
            assert_eq!(frames.iter().filter(|frame| matches!(frame, HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Close { .. }))).count(), if phase == 2 { 2 } else { 1 });
        }
    }
}

/// Canceled Open allocations retain bounded ownership rather than creating an
/// unbounded tombstone map or silently dropping cleanup authority.
#[test]
fn image_canceled_open_capacity_is_bounded_until_late_cleanup() {
    with_transport(|handle, waker| {
        let mut runtime = input_runtime(waker);
        for index in 0..20 {
            let mut call = attached(started(), &[input_png()]);
            call.call_id = ToolCallId::new(format!("call-{index}"));
            runtime
                .start_image_call(call.clone(), handle)
                .expect("start");
            if index < MAX_CALLS {
                reply(&mut runtime, Ok(ArtifactValue::Done), handle);
                runtime
                    .images
                    .tools
                    .cancel(&call.call_id, handle)
                    .expect("cancel");
            }
            assert!(runtime.images.tools.calls.is_empty());
            assert!(runtime.images.tools.opens.len() <= MAX_CALLS);
        }
        assert_eq!(runtime.images.tools.opens.len(), MAX_CALLS);
        for request_id in runtime
            .images
            .tools
            .opens
            .keys()
            .cloned()
            .collect::<Vec<_>>()
        {
            runtime
                .handle_image_artifact_result(
                    ArtifactResult {
                        request_id,
                        result: Ok(ArtifactValue::Opened {
                            read: "late-read".parse().expect("read"),
                            descriptor: descriptor(&input_png()),
                        }),
                    },
                    handle,
                )
                .expect("cleanup");
        }
        runtime
            .start_image_call(attached(started(), &[input_png()]), handle)
            .expect("capacity restored");
        assert_eq!(runtime.images.tools.calls.len(), 1);
        runtime.images.tools.abandon_all();
        assert!(runtime.images.tools.opens.is_empty());
    });
}

/// Aggregate allowance is charged before reading the next descriptor's body;
/// duplicates still consume their full original-byte budget.
#[test]
fn image_original_aggregate_budget_precedes_next_read() {
    let original = vec![0; image_generation::MAX_IMAGE_BYTES];
    let next = vec![0; image_generation::MAX_INPUT_BYTES - original.len() + 1];
    let frames = with_transport(|handle, waker| {
        let mut runtime = input_runtime(waker);
        runtime.images.tools.executor = |_, _, _, _, _, _| panic!("over-budget input executed");
        runtime
            .start_image_call(
                attached(started(), &[original.clone(), next.clone()]),
                handle,
            )
            .expect("start");
        reply(&mut runtime, Ok(ArtifactValue::Done), handle);
        reply(
            &mut runtime,
            Ok(ArtifactValue::Opened {
                read: "first".parse().expect("read"),
                descriptor: descriptor(&original),
            }),
            handle,
        );
        for (index, chunk) in original.chunks(tau_proto::ARTIFACT_CHUNK_BYTES).enumerate() {
            let offset = index * tau_proto::ARTIFACT_CHUNK_BYTES;
            reply(
                &mut runtime,
                Ok(ArtifactValue::Chunk {
                    offset: offset as u64,
                    bytes: chunk.to_vec(),
                    eof: offset + chunk.len() == original.len(),
                }),
                handle,
            );
        }
        reply(&mut runtime, Ok(ArtifactValue::Done), handle);
        reply(
            &mut runtime,
            Ok(ArtifactValue::Opened {
                read: "second".parse().expect("read"),
                descriptor: descriptor(&next),
            }),
            handle,
        );
        assert!(runtime.images.tools.calls.is_empty());
        assert!(runtime.worker_rx.try_recv().is_err());
    });
    assert!(!frames.iter().any(
        |frame| matches!(frame, HarnessInputMessage::ArtifactRequest(request)
        if matches!(&request.op, ArtifactOp::Read { read, .. } if read.as_str() == "second"))
    ));
    assert!(frames.iter().any(
        |frame| matches!(frame, HarnessInputMessage::ArtifactRequest(request)
        if matches!(&request.op, ArtifactOp::Close { read } if read.as_str() == "second"))
    ));
}

/// A canceled finite worker retains capacity and identity until completion, so
/// cancel/restart churn cannot spawn unlimited decoders or adopt a stale
/// result.
#[test]
fn image_canceled_worker_slots_survive_until_completion() {
    with_handle(|handle| {
        let mut tools = images();
        for index in 0..MAX_CALLS {
            let mut pending = call(Phase::Generating);
            pending.started.call_id = ToolCallId::new(format!("worker-{index}"));
            let id = pending.started.call_id.clone();
            tools.workers.insert(id.clone());
            tools.calls.insert(id.clone(), pending);
            tools.cancel(&id, handle).expect("cancel");
            let mut duplicate = started();
            duplicate.call_id = id;
            tools.start(duplicate, handle).expect("ignore duplicate");
            assert!(tools.calls.is_empty());
        }
        tools.start(started(), handle).expect("busy");
        assert!(tools.calls.is_empty());
        for id in tools.workers.iter().cloned().collect::<Vec<_>>() {
            tools
                .generated(id, Ok(input_png()), handle)
                .expect("late completion ignored");
        }
        tools.start(started(), handle).expect("capacity restored");
        assert_eq!(tools.calls.len(), 1);
        tools.abandon_all();
    });
}

/// Storage denial terminalizes before credentials or generation can be loaded.
#[test]
fn image_availability_failure_does_not_generate() {
    let frames = with_handle(|handle| {
        let mut runtime = crate::tests::observation_test_runtime();
        runtime.images.tools = images();
        runtime
            .images
            .tools
            .start(started(), handle)
            .expect("start");
        assert!(matches!(
            runtime
                .images
                .tools
                .calls
                .values()
                .next()
                .expect("call")
                .phase,
            Phase::Availability
        ));
        let request_id = runtime
            .images
            .tools
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
        assert!(runtime.images.tools.calls.is_empty());
        assert!(runtime.worker_rx.try_recv().is_err());
    });
    assert_eq!(frames.iter().filter(|frame| matches!(frame, HarnessInputMessage::ArtifactRequest(request) if request.op == ArtifactOp::Available)).count(), 1);
    assert_eq!(frames.iter().filter(|frame| matches!(frame, HarnessInputMessage::Emit(emit) if matches!(*emit.event, tau_proto::Event::ToolErrorReported(_)))).count(), 1);
}

/// Upload copies every original byte and reports only a canonical artifact
/// reference plus verified descriptor metadata.
#[test]
fn image_original_upload_returns_reference_without_path_or_inline_bytes() {
    let original = vec![0x9b; tau_proto::ARTIFACT_CHUNK_BYTES + 7];
    let descriptor = ArtifactDescriptor::from_bytes(
        &original,
        tau_proto::ArtifactIdentity::metadata(None, Some("image/png".into())).unwrap(),
    )
    .expect("descriptor");
    let frames = with_handle(|handle| {
        let mut runtime = crate::tests::observation_test_runtime();
        runtime
            .images
            .tools
            .calls
            .insert(started().call_id, call(Phase::Generating));
        runtime
            .images
            .tools
            .generated(started().call_id, Ok(original.clone()), handle)
            .expect("generated");
        runtime
            .images
            .tools
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
                .tools
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
        assert!(runtime.images.tools.calls.is_empty());
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
    let display = results[0].display.as_ref().expect("success display");
    assert!(display.args.is_empty());
    assert_eq!(display.stats.bytes, Some(18));
    assert!(display.progress_counters.is_empty());
    assert_eq!(display.status, tau_proto::ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
    assert_eq!(
        display.payload,
        Some(tau_proto::ToolUsePayload::Text {
            text: "Prompt:\na transparent leaf".into(),
        })
    );
    let CborValue::Map(fields) = &results[0].result else {
        panic!("result map");
    };
    assert_eq!(fields.len(), 3);
    assert!(fields.contains(&(
        CborValue::Text("artifact".into()),
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
                runtime.images.tools.calls.insert(started().call_id, call);
                let request_id: ArtifactRequestId = "pending".parse().expect("request");
                runtime
                    .images
                    .tools
                    .requests
                    .insert(request_id.clone(), started().call_id);
                if timeout {
                    runtime
                        .images
                        .tools
                        .timeout(&started().call_id, handle)
                        .expect("timeout");
                } else {
                    runtime
                        .images
                        .tools
                        .cancel(&started().call_id, handle)
                        .expect("cancel");
                }
                assert!(flag.is_cancelled());
                runtime
                    .images
                    .tools
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
                assert!(runtime.images.tools.calls.is_empty());
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
            .tools
            .calls
            .insert(started().call_id, call(Phase::Generating));
        runtime
            .images
            .tools
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
                .tools
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
        assert!(runtime.images.tools.calls.is_empty());
        assert!(runtime.images.tools.requests.is_empty());
        assert!(runtime.worker_rx.try_recv().is_err());
    });
    assert_eq!(frames.iter().filter(|frame| matches!(frame, HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Abort { .. }))).count(), 1);
    assert_eq!(frames.iter().filter(|frame| matches!(frame, HarnessInputMessage::Emit(emit) if matches!(*emit.event, tau_proto::Event::ToolErrorReported(_)))).count(), 1);
}
