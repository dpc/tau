use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicUsize;

use tau_proto::{ArtifactDescriptor, ArtifactRequest, HarnessInputMessage};

use super::*;

/// Completion uses immutable metadata verbatim, never payload classification
/// or a guessed label for legacy missing MIME. MIME and size use only one
/// separating space to preserve completion-menu width.
#[test]
fn completion_displays_recorded_mime_only() {
    let descriptor = ArtifactDescriptor::from_bytes(
        b"not an archive",
        tau_proto::ArtifactIdentity::metadata(None, Some("application/vnd.tau.directory".into()))
            .unwrap(),
    )
    .unwrap();
    let item = row(ArtifactListEntry {
        descriptor,
        last_put_at: 1,
    });
    assert_eq!(item.description, "application/vnd.tau.directory 14 bytes");
    let descriptor = ArtifactDescriptor::from_bytes(
        b"\x89PNG\r\n\x1a\n",
        tau_proto::ArtifactIdentity::LegacyPayload,
    )
    .unwrap();
    let item = row(ArtifactListEntry {
        descriptor,
        last_put_at: 1,
    });
    assert_eq!(item.description, "8 bytes");
}
use crate::chat::{UiIoMeter, UiWriter};

/// Creates an isolated protocol socket; no clipboard or external harness is
/// used.
fn bridge() -> (
    Arc<Rpc>,
    tau_proto::HarnessInputReader<BufReader<UnixStream>>,
) {
    let (ui, peer) = UnixStream::pair().expect("test socket pair");
    peer.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("bounded test read");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui, UiIoMeter::default())));
    (
        Arc::new(Rpc::new(writer, "session".parse().expect("valid session"))),
        tau_proto::HarnessInputReader::new(BufReader::new(peer)),
    )
}

/// Reads only directed artifact frames from the synthetic UI transport.
fn request(reader: &mut tau_proto::HarnessInputReader<BufReader<UnixStream>>) -> ArtifactRequest {
    let Some(HarnessInputMessage::ArtifactRequest(request)) =
        reader.read_message().expect("request frame")
    else {
        panic!("artifact request")
    };
    assert_eq!(request.expected_session_id.as_str(), "session");
    request
}

/// Delivers one correlated response to the ordinary response-demultiplexing
/// seam.
fn reply(rpc: &Rpc, request: ArtifactRequest, value: ArtifactValue) {
    assert!(
        rpc.deliver(ArtifactResult {
            request_id: request.request_id,
            result: Ok(value)
        })
        .is_none()
    );
}

/// The UI must wait for verified Close, not merely the final chunk, before
/// bytes become available to the raw-terminal writer; corruption still closes
/// the read.
#[test]
fn verified_download_waits_for_close_and_rejects_corrupt_originals() {
    for corrupt in [false, true] {
        let (rpc, mut reader) = bridge();
        let bytes = b"exact\0\xfforiginal".to_vec();
        let key: ArtifactKey = blake3::hash(&bytes).to_hex()[..24]
            .to_owned()
            .parse()
            .expect("canonical digest");
        let descriptor =
            ArtifactDescriptor::new(key.clone(), bytes.len() as u64).expect("bounded original");
        let worker_rpc = rpc.clone();
        let (done, result) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let bytes = worker_rpc.download(key, Instant::now() + Duration::from_secs(3), || true);
            done.send(bytes).expect("test receiver remains alive");
        });
        let open = request(&mut reader);
        assert!(matches!(open.op, ArtifactOp::Open { .. }));
        reply(
            &rpc,
            open,
            ArtifactValue::Opened {
                read: "read".parse().expect("valid read identifier"),
                descriptor,
            },
        );
        let read = request(&mut reader);
        assert!(matches!(read.op, ArtifactOp::Read { .. }));
        let mut body = bytes.clone();
        if corrupt {
            body[0] ^= 1;
        }
        reply(
            &rpc,
            read,
            ArtifactValue::Chunk {
                offset: 0,
                bytes: body,
                eof: true,
            },
        );
        let close = request(&mut reader);
        assert!(matches!(close.op, ArtifactOp::Close { .. }));
        if corrupt {
            assert!(
                result
                    .recv_timeout(Duration::from_secs(1))
                    .expect("download completes")
                    .is_err()
            );
        } else {
            assert!(result.try_recv().is_err());
            reply(&rpc, close, ArtifactValue::Done);
            assert_eq!(
                result
                    .recv_timeout(Duration::from_secs(1))
                    .expect("download completes")
                    .expect("verified original"),
                bytes
            );
        }
        rpc.stop();
        worker.join().expect("download worker exits");
    }
}

/// Completion retains only the current prefix's complete bounded page and never
/// accepts a stale response merely because its correlation was valid.
#[test]
fn completion_rejects_old_prefix_and_keeps_full_keys() {
    let (ui, peer) = UnixStream::pair().expect("test socket pair");
    peer.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("bounded test read");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui, UiIoMeter::default())));
    let (control, worker) =
        ClipboardExport::spawn(writer, "session".parse().expect("valid session"));
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(peer));
    let (_term, handle, _input) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term_raw::CursorShape::Bar,
    );
    let complete = control.completer(handle, true);
    assert!(complete(&[""]).is_empty());
    let first = request(&mut reader);
    assert!(matches!(&first.op, ArtifactOp::List { prefix, .. } if prefix.is_empty()));
    assert!(complete(&["a"]).is_empty());
    reply(
        &control.rpc,
        first,
        ArtifactValue::Listed { entries: vec![] },
    );
    let second = request(&mut reader);
    assert!(matches!(&second.op, ArtifactOp::List { prefix, .. } if prefix == "a"));
    let key: ArtifactKey = "a".repeat(24).parse().expect("canonical key");
    reply(
        &control.rpc,
        second,
        ArtifactValue::Listed {
            entries: vec![ArtifactListEntry {
                descriptor: ArtifactDescriptor::new(key.clone(), 14).expect("bounded original"),
                last_put_at: 10,
            }],
        },
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    let rows = loop {
        let rows = complete(&["a"]);
        if !rows.is_empty() {
            break rows;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    };
    assert_eq!(rows[0].value, key.as_str());
    assert!(
        rows[0]
            .label
            .as_ref()
            .expect("abbreviated label")
            .contains('…')
    );
    assert_eq!(rows[0].description, "14 bytes");
    control.stop();
    worker.join().expect("completion worker exits");
}

/// Completion pages must match the requested fixed-width hex prefix, not an old
/// algorithm label or an overlong identity.
#[test]
fn completion_validates_short_results_against_canonical_prefixes() {
    let entries = vec![ArtifactListEntry {
        descriptor: ArtifactDescriptor::new("a".repeat(24).parse().expect("key"), 1)
            .expect("descriptor"),
        last_put_at: 1,
    }];
    for prefix in ["a", "aa"] {
        assert!(valid_page(prefix, &entries), "{prefix}");
    }
    for prefix in ["b", "blake3:b", "blake3:G", &"a".repeat(25)] {
        assert!(!valid_page(prefix, &entries), "{prefix}");
    }
}

/// Reply, stop, and cancellation all wake the sole RPC wait; a cancellation
/// published in the check-to-wait window remains buffered rather than lost.
#[test]
fn rpc_response_stop_and_cancel_use_event_wakes() {
    for action in ["reply", "stop", "cancel"] {
        let (rpc, mut reader) = bridge();
        let live = Arc::new(AtomicBool::new(true));
        let worker_live = live.clone();
        let worker_rpc = rpc.clone();
        let checks = AtomicUsize::new(0);
        let (entered, checked) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::sync_channel(1);
        let (done, result) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let response = worker_rpc.request(
                ArtifactOp::List {
                    prefix: String::new(),
                    limit: 1,
                },
                Instant::now() + Duration::from_secs(30),
                || {
                    let value = worker_live.load(Ordering::Acquire);
                    if checks.fetch_add(1, Ordering::Relaxed) == 1 {
                        entered.send(()).expect("signal check-to-wait window");
                        gate.recv_timeout(Duration::from_secs(2))
                            .expect("release predicate");
                    }
                    value
                },
            );
            done.send(response).expect("test result receiver");
        });
        let frame = request(&mut reader);
        checked
            .recv_timeout(Duration::from_secs(2))
            .expect("request reaches wait");
        match action {
            "reply" => reply(&rpc, frame, ArtifactValue::Listed { entries: vec![] }),
            "stop" => rpc.stop(),
            "cancel" => {
                live.store(false, Ordering::Release);
                rpc.wake.try_send(()).expect("buffer retirement wake");
            }
            _ => unreachable!(),
        }
        release.send(()).expect("release predicate");
        let response = result
            .recv_timeout(Duration::from_secs(1))
            .expect("event wakes request");
        if action == "reply" {
            assert!(matches!(response, Ok(ArtifactValue::Listed { .. })));
        } else {
            assert!(response.is_err());
        }
        worker.join().expect("RPC worker exits");
        rpc.stop();
    }
}

/// A buffered reply is not permission to extend the absolute operation
/// deadline, even when scheduling delays the worker before it consumes that
/// reply.
#[test]
fn rpc_rejects_buffered_response_after_absolute_deadline() {
    let (rpc, mut reader) = bridge();
    let worker_rpc = rpc.clone();
    let deadline = Instant::now() + Duration::from_secs(1);
    let checks = AtomicUsize::new(0);
    let (entered, checked) = mpsc::sync_channel(1);
    let (release, gate) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        worker_rpc.request(
            ArtifactOp::List {
                prefix: String::new(),
                limit: 1,
            },
            deadline,
            || {
                if checks.fetch_add(1, Ordering::Relaxed) == 1 {
                    entered.send(()).expect("signal reply-consumption gate");
                    gate.recv_timeout(Duration::from_secs(3))
                        .expect("release worker");
                }
                true
            },
        )
    });
    let frame = request(&mut reader);
    checked
        .recv_timeout(Duration::from_secs(2))
        .expect("request reaches consumption gate");
    reply(&rpc, frame, ArtifactValue::Listed { entries: vec![] });
    // This is the actual deadline under test, not a synchronization poll.
    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    release.send(()).expect("resume after expiry");
    assert_eq!(
        worker.join().expect("RPC worker exits"),
        Err("Artifact request timed out")
    );
    assert_eq!(
        rpc.request(
            ArtifactOp::List {
                prefix: String::new(),
                limit: 1
            },
            deadline,
            || true
        ),
        Err("Artifact request timed out"),
    );
    rpc.stop();
}
