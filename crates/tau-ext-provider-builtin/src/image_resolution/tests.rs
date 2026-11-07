//! Real Artifact correlation with deterministic byte replies and no live
//! service.

use std::io::{BufReader, Cursor};
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use tau_client::{ExtensionBuilder, TauExtension, TauExtensionRunner};
use tau_proto::{
    ArtifactDescriptor, CborValue, ContextBlock, ContextItem, HarnessInputMessage,
    HarnessOutputMessage, ImageReference, PromptContext, ToolResultContentPart, ToolResultItem,
};

use super::*;
use crate::prewarm::PrewarmAbort as Abort;
use crate::tests::SharedTraceWriter;

/// Minimal real SDK transport with no provider/backend behavior.
struct TestExtension;

impl TauExtension for TestExtension {
    type State = ();
    fn name(&self) -> &'static str {
        "resolver-test"
    }
    fn register(self, _: &mut ExtensionBuilder<()>) {}
}

/// Gives tests a normal writer/waker and returns the actual emitted frames.
pub(crate) fn with_transport(
    run: impl FnOnce(&ClientHandle, ManualRuntimeWaker),
) -> Vec<HarnessInputMessage> {
    let mut input = Vec::new();
    tau_proto::HarnessOutputWriter::new(&mut input)
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            artifact_limits: Default::default(),
            purpose: Default::default(),
            harness_protocol_version: None,
            config: CborValue::Map(Vec::new()),
            instance_name: "resolver-test".parse().expect("instance"),
            tool_prefix: None,
            state_dir: None,
            secrets: Default::default(),
            absent_optional_secrets: Default::default(),
            settings_files: Default::default(),
        }))
        .expect("configure");
    let output = SharedTraceWriter::default();
    let runtime = TauExtensionRunner::new(TestExtension)
        .start_manual_loop(Cursor::new(input), output.clone(), ())
        .expect("runtime");
    run(&runtime.handle(), runtime.waker());
    runtime.finish().expect("finish");
    let bytes = output.bytes();
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(bytes.as_slice()));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("frame") {
        frames.push(frame);
    }
    frames
}

/// One fully encoded, non-animated RGBA PNG, not a magic-byte-only fixture.
pub(crate) fn png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 104, 112, 80, 248,
        15, 0, 4, 4, 1, 224, 242, 235, 128, 249, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

/// Captures the prepared bytes, independently of metadata used by each result.
pub(crate) fn descriptor(bytes: &[u8]) -> ArtifactDescriptor {
    ArtifactDescriptor::new(
        blake3::hash(bytes).to_hex()[..24]
            .to_owned()
            .parse()
            .expect("key"),
        bytes.len() as u64,
    )
    .expect("descriptor")
}

/// Supplies a reference without any original input image in the request.
pub(crate) fn result() -> ToolResultItem {
    ToolResultItem {
        call_id: "image".into(),
        tool_type: tau_proto::ToolType::Function,
        status: tau_proto::ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&CborValue::Null),
        presentation: Default::default(),
        provider_content: vec![ToolResultContentPart::ImageReference(ImageReference {
            blob: descriptor(&png()),
            media_type: tau_proto::ImageMediaType::Png,
            width: 1,
            height: 1,
            detail: tau_proto::ImageDetail::High,
        })],
    }
}

/// A replacement/reference can occur in any typed context block family.
pub(crate) fn context() -> PromptContext {
    PromptContext {
        blocks: vec![
            ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![ContextItem::ToolResult(result())],
            }),
            ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                provider_response_id: None,
                backend: None,
                output_items: vec![ContextItem::ToolResult(result())],
                usage: None,
            }),
            ContextBlock::ToolResults(tau_proto::ToolResultsBlock {
                items: vec![result()],
            }),
        ],
    }
}

/// Answers an admitted RPC through the same correlation gate as real input.
fn answer(
    state: &mut State,
    rpc: Rpc,
    handle: &ClientHandle,
    reply: &mut impl FnMut(&ArtifactOp) -> Result<ArtifactValue, ArtifactError>,
) {
    let live = Arc::clone(&rpc.live);
    state.request(rpc, handle);
    let Some((id, op)) = state
        .requests
        .iter()
        .find_map(|(id, rpc)| Arc::ptr_eq(&rpc.live, &live).then(|| (id.clone(), rpc.op.clone())))
    else {
        return;
    };
    assert!(
        state
            .result(
                ArtifactResult {
                    request_id: id,
                    result: reply(&op)
                },
                handle
            )
            .is_none()
    );
}

/// Supplies normal sequential chunks; the resolver still verifies the digest.
pub(crate) fn artifact_reply(
    op: &ArtifactOp,
    bytes: &[u8],
) -> Result<ArtifactValue, ArtifactError> {
    match op {
        ArtifactOp::Open { .. } => Ok(ArtifactValue::Opened {
            read: "resolver-read".parse().expect("read"),
            descriptor: descriptor(&png()),
        }),
        ArtifactOp::Read { offset, length, .. } => {
            let offset = *offset as usize;
            let end = bytes.len().min(offset + *length as usize);
            Ok(ArtifactValue::Chunk {
                offset: offset as u64,
                bytes: bytes[offset..end].to_vec(),
                eof: end == bytes.len(),
            })
        }
        ArtifactOp::Close { .. } => Ok(ArtifactValue::Done),
        _ => panic!("read-only resolver"),
    }
}

/// Drives the existing runtime mailbox until the finite worker completes.
pub(crate) fn serve<T>(
    state: &mut State,
    receive: &mpsc::Receiver<crate::WorkerMessage>,
    worker: thread::JoinHandle<T>,
    handle: &ClientHandle,
    mut reply: impl FnMut(&ArtifactOp) -> Result<ArtifactValue, ArtifactError>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !worker.is_finished() {
        assert!(Instant::now() < deadline, "resolver worker did not finish");
        if let Ok(crate::WorkerMessage::ImageResolveRpc(rpc)) =
            receive.recv_timeout(Duration::from_millis(10))
        {
            answer(state, rpc, handle, &mut reply);
        }
    }
    let output = worker.join().expect("worker");
    while let Ok(crate::WorkerMessage::ImageResolveRpc(rpc)) = receive.try_recv() {
        answer(state, rpc, handle, &mut reply);
    }
    output
}

/// All block families and later warm requests share one verified allocation,
/// while metadata remains attached to each independent occurrence.
#[test]
fn resolver_cold_download_and_warm_cache_share_verified_bytes() {
    let frames = with_transport(|handle, waker| {
        let mut state = State::default();
        let (tx, rx) = mpsc::channel();
        let client = state.client(tx, waker);
        let worker = thread::spawn(move || {
            let mut first = context();
            client
                .resolve(
                    &mut first,
                    &"s1".parse().expect("session"),
                    &mut Abort::default(),
                )
                .expect("cold");
            let mut second = context();
            client
                .resolve(
                    &mut second,
                    &"s1".parse().expect("session"),
                    &mut Abort::default(),
                )
                .expect("warm");
            assert!(!first.has_image_references());
            let images: Vec<_> = first
                .flatten_iter()
                .chain(second.flatten_iter())
                .filter_map(|item| {
                    let ContextItem::ToolResult(result) = item else {
                        return None;
                    };
                    Some(Arc::clone(
                        &result.provider_content[0]
                            .inline_image()
                            .expect("inline")
                            .data,
                    ))
                })
                .collect();
            assert_eq!(images.len(), 6);
            assert!(
                images
                    .iter()
                    .all(|bytes| bytes.as_ref() == png() && Arc::ptr_eq(bytes, &images[0]))
            );
        });
        serve(&mut state, &rx, worker, handle, |op| {
            artifact_reply(op, &png())
        });
    });
    assert_eq!(frames.iter().filter(|frame| matches!(frame,
        HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Open { .. }))).count(), 1);
}

/// Missing/corrupt backing fails only its consuming request; a healthy request
/// can subsequently use the same resolver and normal correlation loop.
#[test]
fn resolver_failures_do_not_cache_partial_or_unverified_bytes() {
    for missing in [true, false] {
        with_transport(|handle, waker| {
            let mut state = State::default();
            let (tx, rx) = mpsc::channel();
            let client = state.client(tx, waker);
            let first = client.clone();
            let worker = thread::spawn(move || {
                first.resolve(
                    &mut context(),
                    &"s1".parse().expect("session"),
                    &mut Abort::default(),
                )
            });
            let error = serve(&mut state, &rx, worker, handle, |op| {
                if missing {
                    Err(ArtifactError::Unavailable)
                } else {
                    let mut bytes = png();
                    bytes[0] ^= 1;
                    artifact_reply(op, &bytes)
                }
            });
            assert!(error.is_err());
            let worker = thread::spawn(move || {
                client.resolve(
                    &mut context(),
                    &"s1".parse().expect("session"),
                    &mut Abort::default(),
                )
            });
            serve(&mut state, &rx, worker, handle, |op| {
                artifact_reply(op, &png())
            })
            .expect("next request");
        });
    }
}

/// A verified cache hit cannot bless altered size, MIME or decoded dimensions.
#[test]
fn resolver_validates_each_cached_occurrence_metadata() {
    for damage in ["size", "mime", "dimensions"] {
        with_transport(|handle, waker| {
            let mut state = State::default();
            let (tx, rx) = mpsc::channel();
            let client = state.client(tx, waker);
            let worker = thread::spawn(move || {
                client
                    .resolve(
                        &mut context(),
                        &"s1".parse().expect("session"),
                        &mut Abort::default(),
                    )
                    .expect("warm");
                let mut result = result();
                let ToolResultContentPart::ImageReference(image) = &mut result.provider_content[0]
                else {
                    unreachable!()
                };
                match damage {
                    "size" => {
                        image.blob.size =
                            tau_proto::ArtifactSize::new(png().len() as u64 + 1).expect("size")
                    }
                    "mime" => image.media_type = tau_proto::ImageMediaType::Jpeg,
                    _ => image.width += 1,
                }
                let mut context = PromptContext {
                    blocks: vec![ContextBlock::ToolResults(tau_proto::ToolResultsBlock {
                        items: vec![result],
                    })],
                };
                assert!(
                    client
                        .resolve(
                            &mut context,
                            &"s1".parse().expect("session"),
                            &mut Abort::default()
                        )
                        .is_err()
                );
            });
            serve(&mut state, &rx, worker, handle, |op| {
                artifact_reply(op, &png())
            });
        });
    }
}

/// Canceled Open replies retain cleanup authority both before response routing
/// and after a successful channel send that races the worker's cancellation.
#[test]
fn resolver_cancellation_closes_late_and_queued_open_allocations() {
    for queued in [false, true] {
        let frames = with_transport(|handle, waker| {
            let mut state = State::default();
            let (tx, rx) = mpsc::channel();
            let client = state.client(tx, waker);
            let abort = Abort::default();
            let mut worker_abort = abort.clone();
            let worker = thread::spawn(move || {
                client.resolve(
                    &mut context(),
                    &"s1".parse().expect("session"),
                    &mut worker_abort,
                )
            });
            let crate::WorkerMessage::ImageResolveRpc(rpc) =
                rx.recv_timeout(Duration::from_secs(3)).expect("open")
            else {
                panic!("rpc")
            };
            state.request(rpc, handle);
            abort.cancel();
            if !queued {
                assert!(worker.join().expect("worker").is_err());
            } else {
                // Routing now races the already-observed cancellation flag.
                let id = state.requests.keys().next().expect("request").clone();
                state.result(
                    ArtifactResult {
                        request_id: id,
                        result: artifact_reply(
                            &ArtifactOp::Open {
                                key: descriptor(&png()).key,
                            },
                            &png(),
                        ),
                    },
                    handle,
                );
                assert!(worker.join().expect("worker").is_err());
            }
            let pending: Vec<_> = state.requests.keys().cloned().collect();
            for id in pending {
                state.result(
                    ArtifactResult {
                        request_id: id,
                        result: artifact_reply(
                            &ArtifactOp::Open {
                                key: descriptor(&png()).key,
                            },
                            &png(),
                        ),
                    },
                    handle,
                );
            }
            for message in rx.try_iter() {
                if let crate::WorkerMessage::ImageResolveRpc(rpc) = message {
                    answer(&mut state, rpc, handle, &mut |op| {
                        artifact_reply(op, &png())
                    });
                }
            }
        });
        assert_eq!(frames.iter().filter(|frame| matches!(frame,
            HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Close { .. }))).count(), 1);
    }
}

/// Concurrent consumers share one physical download and retain their own
/// cancellable wait rather than holding a cache lock through RPC.
#[test]
fn resolver_concurrent_consumers_share_one_download() {
    let frames = with_transport(|handle, waker| {
        let mut state = State::default();
        let (tx, rx) = mpsc::channel();
        let client = state.client(tx, waker);
        let start = Arc::new(Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let client = client.clone();
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    client.resolve(
                        &mut context(),
                        &"s1".parse().expect("session"),
                        &mut Abort::default(),
                    )
                })
            })
            .collect();
        start.wait();
        let joined = thread::spawn(move || {
            for worker in workers {
                worker.join().expect("worker").expect("resolved");
            }
        });
        serve(&mut state, &rx, joined, handle, |op| {
            artifact_reply(op, &png())
        });
    });
    assert_eq!(frames.iter().filter(|frame| matches!(frame,
        HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Open { .. }))).count(), 1);
}

/// Canceled unresolved allocations remain charged against the same eight RPC
/// slots until their response can close the read; no unbounded cleanup queue.
#[test]
fn resolver_rpc_limit_includes_canceled_allocations_and_preserves_demultiplexing() {
    with_transport(|handle, _| {
        let mut state = State::default();
        let mut receivers = Vec::new();
        for _ in 0..MAX_TRANSFERS {
            let (reply, receive) = mpsc::sync_channel(1);
            let live = Arc::new(AtomicBool::new(true));
            state.request(
                Rpc {
                    session: "s1".parse().expect("session"),
                    op: ArtifactOp::Open {
                        key: descriptor(&png()).key,
                    },
                    reply,
                    live: Arc::clone(&live),
                },
                handle,
            );
            live.store(false, Ordering::Release);
            receivers.push(receive);
        }
        let (reply, receive) = mpsc::sync_channel(1);
        state.request(
            Rpc {
                session: "s1".parse().expect("session"),
                op: ArtifactOp::Open {
                    key: descriptor(&png()).key,
                },
                reply,
                live: Arc::new(AtomicBool::new(true)),
            },
            handle,
        );
        let Signal::Reply(reply) = receive.recv().expect("local overload") else {
            panic!("reply")
        };
        assert!(matches!(reply.take(), Err(ArtifactError::Unavailable)));
        assert_eq!(state.requests.len(), MAX_TRANSFERS);
        let unowned = ArtifactResult {
            request_id: "image-tool-rpc".parse().expect("id"),
            result: Ok(ArtifactValue::Done),
        };
        assert_eq!(state.result(unowned.clone(), handle), Some(unowned));
        for id in state.requests.keys().cloned().collect::<Vec<_>>() {
            state.result(
                ArtifactResult {
                    request_id: id,
                    result: artifact_reply(
                        &ArtifactOp::Open {
                            key: descriptor(&png()).key,
                        },
                        &png(),
                    ),
                },
                handle,
            );
        }
        assert!(state.requests.is_empty());
    });
}

/// A successfully queued Open reply still owns cleanup when its receiver is
/// abandoned; testing this exact cut avoids relying on a scheduling race.
#[test]
fn resolver_queued_allocation_reply_closes_when_receiver_is_dropped() {
    let frames = with_transport(|handle, _| {
        let mut state = State::default();
        let (reply, receive) = mpsc::sync_channel(1);
        state.request(
            Rpc {
                session: "s1".parse().expect("session"),
                op: ArtifactOp::Open {
                    key: descriptor(&png()).key,
                },
                reply,
                live: Arc::new(AtomicBool::new(true)),
            },
            handle,
        );
        let id = state.requests.keys().next().expect("request").clone();
        state.result(
            ArtifactResult {
                request_id: id,
                result: artifact_reply(
                    &ArtifactOp::Open {
                        key: descriptor(&png()).key,
                    },
                    &png(),
                ),
            },
            handle,
        );
        drop(receive);
    });
    assert_eq!(frames.iter().filter(|frame| matches!(frame,
        HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Close { .. }))).count(), 1);
}

/// Session/transport shutdown revokes both cold workers and previously verified
/// cache hits instead of submitting a stale prompt to a model.
#[test]
fn resolver_shutdown_revokes_cached_consumption() {
    with_transport(|handle, waker| {
        let mut state = State::default();
        let (tx, rx) = mpsc::channel();
        let client = state.client(tx, waker);
        let cold = client.clone();
        let worker = thread::spawn(move || {
            cold.resolve(
                &mut context(),
                &"s1".parse().expect("session"),
                &mut Abort::default(),
            )
        });
        serve(&mut state, &rx, worker, handle, |op| {
            artifact_reply(op, &png())
        })
        .expect("cold");
        state.shutdown();
        assert!(
            client
                .resolve(
                    &mut context(),
                    &"s1".parse().expect("session"),
                    &mut Abort::default()
                )
                .is_err()
        );
        assert!(
            rx.try_recv().is_err(),
            "shutdown cache hit must not dispatch"
        );
    });
}

/// Both a silent RPC and a duplicate-key cache wait terminate at their original
/// deadline without periodic internal wakeups or loss of late Open cleanup.
#[test]
fn resolver_deadlines_bound_silent_rpc_and_cache_waits() {
    for duplicate in [false, true] {
        with_transport(|handle, waker| {
            let mut state = State::default();
            let (tx, rx) = mpsc::channel();
            let client = state.client(tx, waker);
            let abort = Abort::default();
            let mut worker_abort = abort.clone();
            let first = client.clone();
            let worker = thread::spawn(move || {
                let deadline = Instant::now()
                    + if duplicate {
                        Duration::from_secs(5)
                    } else {
                        Duration::from_millis(250)
                    };
                first.resolve_until(
                    &mut context(),
                    &"s1".parse().expect("session"),
                    &mut worker_abort,
                    deadline,
                )
            });
            let crate::WorkerMessage::ImageResolveRpc(rpc) =
                rx.recv_timeout(Duration::from_secs(2)).expect("open")
            else {
                panic!("rpc")
            };
            state.request(rpc, handle);
            if duplicate {
                let waiting = thread::spawn(move || {
                    client.resolve_until(
                        &mut context(),
                        &"s1".parse().expect("session"),
                        &mut Abort::default(),
                        Instant::now() + Duration::from_millis(250),
                    )
                });
                assert!(waiting.join().expect("cache waiter").is_err());
                assert!(
                    rx.try_recv().is_err(),
                    "duplicate must await the owning download"
                );
                abort.cancel();
            }
            assert!(worker.join().expect("RPC worker").is_err());
            assert!(state.cache.inner.lock().expect("cache").fetching.is_empty());
            for id in state.requests.keys().cloned().collect::<Vec<_>>() {
                state.result(
                    ArtifactResult {
                        request_id: id,
                        result: artifact_reply(
                            &ArtifactOp::Open {
                                key: descriptor(&png()).key,
                            },
                            &png(),
                        ),
                    },
                    handle,
                );
            }
            assert!(state.requests.is_empty());
        });
    }
}

/// Harness Busy is an external response with no capacity wake; bounded local
/// backoff retries only the artifact operation, never a provider attempt.
#[test]
fn resolver_external_busy_retries_without_losing_correlation() {
    let frames = with_transport(|handle, waker| {
        let mut state = State::default();
        let (tx, rx) = mpsc::channel();
        let client = state.client(tx, waker);
        let worker = thread::spawn(move || {
            client.resolve(
                &mut context(),
                &"s1".parse().expect("session"),
                &mut Abort::default(),
            )
        });
        let mut busy = true;
        serve(&mut state, &rx, worker, handle, |op| {
            if std::mem::take(&mut busy) {
                Err(ArtifactError::Busy)
            } else {
                artifact_reply(op, &png())
            }
        })
        .expect("bounded retry");
    });
    assert_eq!(frames.iter().filter(|frame| matches!(frame,
        HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Open { .. }))).count(), 2);
}

/// Cancellation interrupts a blocked Read through its registered wake and
/// releases the already-known transfer without waiting for another chunk.
#[test]
fn resolver_read_cancellation_wakes_and_closes_known_transfer() {
    let frames = with_transport(|handle, waker| {
        let mut state = State::default();
        let (tx, rx) = mpsc::channel();
        let client = state.client(tx, waker);
        let abort = Abort::default();
        let mut worker_abort = abort.clone();
        let worker = thread::spawn(move || {
            client.resolve(
                &mut context(),
                &"s1".parse().expect("session"),
                &mut worker_abort,
            )
        });
        let crate::WorkerMessage::ImageResolveRpc(open) =
            rx.recv_timeout(Duration::from_secs(2)).expect("open")
        else {
            panic!("rpc")
        };
        answer(&mut state, open, handle, &mut |op| {
            artifact_reply(op, &png())
        });
        let crate::WorkerMessage::ImageResolveRpc(read) =
            rx.recv_timeout(Duration::from_secs(2)).expect("read")
        else {
            panic!("rpc")
        };
        assert!(matches!(read.op, ArtifactOp::Read { .. }));
        state.request(read, handle);
        abort.cancel();
        assert!(worker.join().expect("worker").is_err());
        let crate::WorkerMessage::ImageResolveRpc(close) =
            rx.recv_timeout(Duration::from_secs(2)).expect("close")
        else {
            panic!("rpc")
        };
        assert!(matches!(close.op, ArtifactOp::Close { .. }));
        answer(&mut state, close, handle, &mut |op| {
            artifact_reply(op, &png())
        });
    });
    assert_eq!(frames.iter().filter(|frame| matches!(frame,
        HarnessInputMessage::ArtifactRequest(request) if matches!(request.op, ArtifactOp::Close { .. }))).count(), 1);
}
