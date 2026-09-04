use std::io::{BufReader, BufWriter, Read as _};
use std::os::unix::net::UnixListener;
use std::sync::mpsc;

use tau_proto::{HarnessInputReader, HarnessOutputWriter, SessionStartReason, SessionStarted};

use super::*;

/// Ensures a subscribed observer keeps its first accepted connection while
/// delayed replay is gated, preventing the archived conn-1 EOF, conn-2
/// reconnect, and daemon `session.shutdown` failure shape.
#[test]
fn delayed_session_replay_keeps_one_accepted_observer_connection() {
    let tempdir = tempfile::TempDir::new().expect("temporary observer root");
    let socket = tempdir.path().join("observer.sock");
    let listener = UnixListener::bind(&socket).expect("bind observer listener");
    let reconnect_probe = listener.try_clone().expect("clone listener");
    let (subscribed_tx, subscribed_rx) = mpsc::sync_channel(1);
    let (probe_tx, probe_rx) = mpsc::sync_channel(1);
    let (replay_release_tx, replay_release_rx) = mpsc::sync_channel(1);
    let (wait_deadline_tx, wait_deadline_rx) = mpsc::sync_channel(1);
    let (wait_release_tx, wait_release_rx) = mpsc::sync_channel(1);
    let (peer_open_tx, peer_open_rx) = mpsc::sync_channel(1);
    let expected = SessionId::parse("observer-delayed-replay").expect("valid session id");
    let server_expected = expected.clone();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept observer");
        let mut reader =
            HarnessInputReader::new(BufReader::new(stream.try_clone().expect("clone stream")));
        assert!(matches!(
            reader.read_message().expect("read hello"),
            Some(HarnessInputMessage::Hello(_))
        ));
        let mut writer =
            HarnessOutputWriter::new(BufWriter::new(stream.try_clone().expect("clone writer")));
        writer
            .write_message(&HarnessOutputMessage::SessionAccepted(
                tau_proto::SessionAccepted {
                    session_id: server_expected.clone(),
                },
            ))
            .expect("write acceptance");
        writer.flush().expect("flush acceptance");
        assert!(matches!(
            reader.read_message().expect("read subscribe"),
            Some(HarnessInputMessage::Subscribe(_))
        ));
        subscribed_tx.send(()).expect("report subscription");
        probe_rx.recv().expect("request open-peer probe");
        stream
            .set_nonblocking(true)
            .expect("set observer stream nonblocking");
        let mut byte = [0_u8; 1];
        peer_open_tx
            .send(matches!(
                stream.try_clone().expect("clone probe stream").read(&mut byte),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
            ))
            .expect("report open peer");
        replay_release_rx.recv().expect("release replay");
        let mut writer = HarnessOutputWriter::new(BufWriter::new(stream));
        writer
            .write_message(&HarnessOutputMessage::deliver_replay(
                UnixMicros::new(1),
                Event::SessionStarted(SessionStarted {
                    session_id: server_expected,
                    reason: SessionStartReason::Resume,
                }),
            ))
            .expect("write delayed replay");
        writer.flush().expect("flush delayed replay");
    });
    let observer_expected = expected.clone();
    let artifact = tempdir.path().join("observer.json");
    let absolute_deadline = Instant::now() + Duration::from_secs(5);
    let observer = thread::spawn(move || {
        SideObserver::connect_with_prompt_drafts_using_replay_receiver(
            &socket,
            &observer_expected,
            artifact,
            absolute_deadline,
            false,
            |observer, selected_deadline| {
                wait_deadline_tx
                    .send(selected_deadline)
                    .expect("report replay wait deadline");
                wait_release_rx.recv().expect("release replay wait");
                observer.recv_one(selected_deadline)
            },
        )
        .map(|observer| observer.events)
        .map_err(|error| error.to_string())
    });

    subscribed_rx.recv().expect("observer subscribed");
    assert_eq!(
        wait_deadline_rx
            .recv()
            .expect("receive replay wait deadline"),
        absolute_deadline,
        "observer replaced the caller's absolute deadline with an intermediate retry boundary"
    );
    probe_tx.send(()).expect("request peer probe");
    assert!(
        peer_open_rx.recv().expect("receive peer probe"),
        "accepted observer disconnected before delayed replay"
    );
    reconnect_probe
        .set_nonblocking(true)
        .expect("set reconnect probe nonblocking");
    assert!(
        matches!(
            reconnect_probe.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ),
        "observer opened the archived second connection while replay was delayed"
    );
    replay_release_tx.send(()).expect("release delayed replay");
    wait_release_tx.send(()).expect("release replay wait");
    let events = observer
        .join()
        .expect("observer thread joins")
        .expect("observer accepts delayed replay");
    server.join().expect("server thread joins");
    assert!(matches!(
        events.as_slice(),
        [ObservedEvent {
            event: Event::SessionStarted(started),
            replay: true,
            recorded_at: _,
        }] if started.session_id == expected
    ));
}

/// Ensures an already-expired caller deadline fails the accepted observer
/// connection exactly once instead of starting a fresh connection window.
#[test]
fn expired_session_replay_deadline_does_not_reconnect() {
    let tempdir = tempfile::TempDir::new().expect("temporary observer root");
    let socket = tempdir.path().join("observer.sock");
    let listener = UnixListener::bind(&socket).expect("bind observer listener");
    let reconnect_probe = listener.try_clone().expect("clone listener");
    let server_expected = SessionId::parse("observer-expired-replay").expect("valid session id");
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept observer");
        let mut reader =
            HarnessInputReader::new(BufReader::new(stream.try_clone().expect("clone stream")));
        assert!(matches!(
            reader.read_message().expect("read hello"),
            Some(HarnessInputMessage::Hello(_))
        ));
        let mut writer = HarnessOutputWriter::new(BufWriter::new(stream));
        writer
            .write_message(&HarnessOutputMessage::SessionAccepted(
                tau_proto::SessionAccepted {
                    session_id: server_expected,
                },
            ))
            .expect("write acceptance");
        writer.flush().expect("flush acceptance");
        assert!(matches!(
            reader.read_message().expect("read subscribe"),
            Some(HarnessInputMessage::Subscribe(_))
        ));
        std::thread::sleep(Duration::from_millis(150));
    });
    let expected = SessionId::parse("observer-expired-replay").expect("valid session id");
    let error = match SideObserver::connect(
        &socket,
        &expected,
        tempdir.path().join("observer.json"),
        Instant::now() + Duration::from_millis(100),
    ) {
        Ok(_) => panic!("expired replay deadline must fail"),
        Err(error) => error,
    };
    server.join().expect("server thread joins");
    assert_eq!(
        error.to_string(),
        "timed out waiting for side-observer event while waiting for side-observer \
         SessionStarted for `observer-expired-replay`"
    );
    reconnect_probe
        .set_nonblocking(true)
        .expect("set reconnect probe nonblocking");
    assert!(
        matches!(
            reconnect_probe.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ),
        "expired accepted observer opened a second connection"
    );
}
