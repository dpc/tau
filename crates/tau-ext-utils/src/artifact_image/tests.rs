//! Regression coverage for artifact image lifecycle helpers.

use std::io::{BufReader, Cursor};
#[cfg(unix)]
use std::net::Shutdown;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(unix)]
use std::thread;

use tau_proto::{
    AgentId, CborValue, Configure, Event, ExtensionDataRequestOp, ExtensionDataResultPayload,
    ExtensionDataScope, ExtensionDataValue, HarnessInputMessage, HarnessInputReader,
    HarnessOutputMessage, HarnessOutputWriter, ImageContent, ImageDetail, ImageMediaType,
    PromptOriginator, ToolCallId, ToolResult, ToolResultContentPart, ToolResultKind, ToolStarted,
    ToolType, UnixMicros,
};

use super::{ActiveCall, DecodeCompletion, IMAGE_TOO_LARGE_MESSAGE, budget_prepared_result};
use crate::read_image::ReadImageRequest;
use crate::{TimerRuntime, UtilsExtension, read_initial_config, send_startup};

/// Build one string-keyed CBOR map for focused protocol fixtures.
fn cbor_map(entries: Vec<(&str, CborValue)>) -> CborValue {
    CborValue::Map(
        entries
            .into_iter()
            .map(|(key, value)| (CborValue::Text(key.to_owned()), value))
            .collect(),
    )
}

/// Ensures an image whose complete transient terminal exceeds the shared frame
/// cap becomes one byte-free error instead of failing the extension writer.
#[test]
fn oversized_prepared_image_becomes_byte_free_error() {
    let result = ToolResult {
        presentation: Default::default(),
        call_id: tau_proto::ToolCallId::new("oversized"),
        tool_name: tau_proto::ToolName::new("read_image"),
        tool_type: ToolType::Function,
        result: CborValue::Text("metadata".to_owned()),
        provider_content: vec![ToolResultContentPart::Image(ImageContent {
            media_type: ImageMediaType::Png,
            data: Arc::from(vec![0; tau_client::MAX_OUTBOUND_FRAME_BYTES as usize]),
            width: 1,
            height: 1,
            detail: ImageDetail::High,
        })],
        kind: ToolResultKind::Final,
        display: None,
        originator: Default::default(),
    };
    let error = budget_prepared_result(result)
        .expect("measure terminal")
        .expect_err("oversized image becomes error");
    assert_eq!(error.message, IMAGE_TOO_LARGE_MESSAGE);
    assert!(error.details.is_none());
    let message =
        tau_proto::HarnessInputMessage::emit_with_persist(Event::ToolErrorReported(error), false);
    assert!(
        tau_client::encoded_outbound_frame_bytes(&message).expect("measure error")
            <= tau_client::MAX_OUTBOUND_FRAME_BYTES
    );
}

/// Ensures the real utility loop handles a deferred live cancellation before an
/// already-ready decoder completion, so only the cancellation terminal
/// survives.
#[cfg(unix)]
#[test]
fn queued_cancellation_precedes_ready_decoder_completion() {
    let source = image::DynamicImage::new_rgb8(4, 3);
    let mut encoded = Cursor::new(Vec::new());
    source
        .write_to(&mut encoded, image::ImageFormat::Png)
        .expect("encode PNG");
    let bytes = encoded.into_inner();
    let key = tau_proto::ArtifactKey::parse(format!("blake3:{}", blake3::hash(&bytes).to_hex()))
        .expect("artifact key");
    let call_id = ToolCallId::new("queued-cancel");
    let invoke = ToolStarted {
        invocation_policy: Default::default(),
        call_id: call_id.clone(),
        tool_name: tau_proto::ToolName::new("work_read_image"),
        arguments: cbor_map(vec![("key", CborValue::Text(key.to_string()))]),
        agent_id: AgentId::parse("agent-image").expect("agent"),
        originator: PromptOriginator::User,
    };
    let request = ReadImageRequest::from_arguments(&invoke.arguments).expect("read_image request");
    let prepared = request
        .prepare(&bytes, bytes.len() as u64)
        .expect("prepare image");

    let (harness, extension) = UnixStream::pair().expect("socket pair");
    let extension_reader = extension.try_clone().expect("clone extension socket");
    let harness_reader = harness.try_clone().expect("clone harness socket");
    let harness_shutdown = harness.try_clone().expect("clone shutdown socket");
    let harness_done = thread::spawn(move || {
        let mut reader = HarnessInputReader::new(BufReader::new(harness_reader));
        let mut writer = HarnessOutputWriter::new(harness);
        assert!(matches!(
            reader.read_message().expect("hello").expect("hello frame"),
            HarnessInputMessage::Hello(_)
        ));
        writer
            .write_message(&HarnessOutputMessage::Configure(Configure {
                purpose: tau_proto::ConfigurePurpose::Runtime,
                tool_prefix: Some(tau_proto::ToolNamePrefix::parse("work").expect("prefix")),
                instance_name: tau_proto::ExtensionName::parse("std-utils").expect("instance"),
                config: cbor_map(vec![(
                    "papercut",
                    cbor_map(vec![("enable", CborValue::Bool(false))]),
                )]),
                state_dir: None,
                secrets: Default::default(),
                settings_files: Default::default(),
            }))
            .expect("configure");
        writer.flush().expect("flush configure");
        loop {
            if matches!(
                reader
                    .read_message()
                    .expect("startup output")
                    .expect("startup frame"),
                HarnessInputMessage::Ready(_)
            ) {
                break;
            }
        }
        let request = loop {
            let frame = reader
                .read_message()
                .expect("barrier request")
                .expect("barrier frame");
            if let HarnessInputMessage::ExtensionDataRequest(request) = frame {
                break request;
            }
        };
        writer
            .write_message(&HarnessOutputMessage::Deliver(
                tau_proto::EventDelivery::live(
                    UnixMicros::new(1),
                    Event::ToolCancelled(tau_proto::ToolCancelled {
                        call_id: ToolCallId::new("queued-cancel"),
                        tool_name: tau_proto::ToolName::new("work_read_image"),
                        tool_type: ToolType::Function,
                        presentation: Default::default(),
                        display: None,
                    }),
                ),
            ))
            .expect("queued cancellation");
        writer
            .write_message(&HarnessOutputMessage::ExtensionDataResult(Box::new(
                tau_proto::ExtensionDataResult {
                    request_id: request.request_id,
                    result: ExtensionDataResultPayload::Ok {
                        value: ExtensionDataValue::DeleteFile,
                    },
                },
            )))
            .expect("barrier result");
        writer.flush().expect("flush cancellation barrier");

        let mut cancelled = 0;
        let mut succeeded = 0;
        let mut errored = 0;
        let mut closed_input = false;
        while let Some(frame) = reader.read_message().expect("terminal output") {
            let HarnessInputMessage::Emit(emit) = frame else {
                continue;
            };
            match emit.event.as_ref() {
                Event::ToolCancelledReported(terminal)
                    if terminal.call_id.as_str() == "queued-cancel" =>
                {
                    cancelled += 1;
                }
                Event::ToolResultReported(terminal)
                    if terminal.call_id.as_str() == "queued-cancel" =>
                {
                    succeeded += 1;
                }
                Event::ToolErrorReported(terminal)
                    if terminal.call_id.as_str() == "queued-cancel" =>
                {
                    errored += 1;
                }
                _ => continue,
            }
            if !closed_input {
                harness_shutdown
                    .shutdown(Shutdown::Write)
                    .expect("close harness input");
                closed_input = true;
            }
        }
        (cancelled, succeeded, errored)
    });

    let mut runtime = tau_client::TauExtensionRunner::new(UtilsExtension)
        .start_manual_loop_deferred_startup_with_state(
            extension_reader,
            extension,
            TimerRuntime::new,
        )
        .expect("start deferred utility runtime");
    let configure = read_initial_config(&mut runtime)
        .expect("receive configuration")
        .expect("runtime configuration");
    let config = configure
        .config
        .deserialized::<crate::UtilsConfig>()
        .expect("utility configuration");
    send_startup(&mut runtime, config.papercut.enable).expect("utility startup");
    runtime
        .extension_data_client()
        .request(
            ExtensionDataScope::User,
            ExtensionDataRequestOp::DeleteFile {
                path: tau_proto::ExtensionDataPath::new("test-barrier"),
            },
        )
        .expect("establish deferred cancellation");

    let images = runtime
        .state_mut()
        .artifact_images
        .as_mut()
        .expect("image manager");
    images.calls.insert(
        call_id.clone(),
        ActiveCall::Decoding {
            invoke,
            cancelled: Arc::new(AtomicBool::new(false)),
        },
    );
    images.decode_active = true;
    images
        .completions
        .lock()
        .expect("completion queue")
        .push_back(DecodeCompletion {
            call_id,
            result: Ok(prepared),
        });

    TimerRuntime::run(runtime).expect("run utility loop");
    let (cancelled, succeeded, errored) = harness_done.join().expect("harness thread");
    assert_eq!(cancelled, 1);
    assert_eq!(succeeded, 0);
    assert_eq!(errored, 0);
}
