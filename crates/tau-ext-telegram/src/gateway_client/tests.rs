use std::cell::Cell;
use std::io as path_std_io;
use std::io::{BufRead, ErrorKind, Write};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::test_support::*;
use super::*;

/// Return a unique local socket path for gateway-client protocol tests.
fn socket_path() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "tau-telegram-gateway-client-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Run a one-response fake gateway and return a connected client.
fn connect_error_with_response(response: String) -> String {
    let path = socket_path();
    let listener = UnixListener::bind(&path).expect("bind fake gateway");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        let mut line = String::new();
        path_std_io::BufReader::new(stream.try_clone().expect("clone stream"))
            .read_line(&mut line)
            .expect("read request");
        writeln!(stream, "{response}").expect("write response");
    });
    let client = GatewayClient::new(GatewayClientConfig::for_test(path));
    match client.connect_cancellable(|| false) {
        Ok(_) => panic!("test caller expects connect failure"),
        Err(error) => error.to_string(),
    }
}

/// Gateway-client socket responses must use the expected protocol version
/// so incompatible local daemons fail closed instead of being parsed
/// loosely.
#[test]
fn protocol_version_mismatch_is_rejected() {
    let error = connect_error_with_response(
        serde_json::json!({
            "protocol_version": 999,
            "ok": true,
            "deliveries": [],
        })
        .to_string(),
    );
    assert!(error.contains("protocol version"), "{error}");
}

/// Gateway-client responses must include the current protocol version rather
/// than silently substituting one.
#[test]
fn missing_protocol_version_is_rejected() {
    let error = connect_error_with_response(
        serde_json::json!({
            "ok": true,
            "deliveries": [],
        })
        .to_string(),
    );
    assert!(error.contains("protocol_version"), "{error}");
}

/// Gateway `ok:false` responses surface the provided error instead of being
/// treated as successful lease or delivery acknowledgements.
#[test]
fn gateway_error_response_is_rejected() {
    let path = socket_path();
    let listener = UnixListener::bind(&path).expect("bind fake gateway");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        let mut line = String::new();
        path_std_io::BufReader::new(stream.try_clone().expect("clone stream"))
            .read_line(&mut line)
            .expect("read request");
        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "protocol_version": SOCKET_PROTOCOL_VERSION,
                "ok": false,
                "error": "denied",
            })
        )
        .expect("write response");
    });
    let client = GatewayClient::new(GatewayClientConfig::for_test(path));
    let error = match client.connect_cancellable(|| false) {
        Ok(_) => panic!("error response should fail"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "denied");
}

/// Gateway response lines are bounded so a broken same-UID socket peer
/// cannot force unbounded buffering in the sidecar.
#[test]
fn oversized_gateway_response_is_rejected() {
    let error = connect_error_with_response("x".repeat(MAX_GATEWAY_RESPONSE_BYTES + 1));
    assert!(error.contains("too large"), "{error}");
}

/// The sidecar honors the gateway-advertised heartbeat interval, clamped to
/// a non-zero delay for malformed zero values.
#[test]
fn heartbeat_interval_is_updated_from_response() {
    let path = socket_path();
    let listener = UnixListener::bind(&path).expect("bind fake gateway");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        authenticate_test_gateway(&mut stream, &"44".repeat(32));
        let mut line = String::new();
        path_std_io::BufReader::new(stream.try_clone().expect("clone stream"))
            .read_line(&mut line)
            .expect("read heartbeat");
        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "protocol_version": SOCKET_PROTOCOL_VERSION,
                "ok": true,
                "gateway_generation": "44".repeat(32),
                "heartbeat_interval_seconds": 2,
                "deliveries": [],
            })
        )
        .expect("write response");
    });
    let client = GatewayClient::new(GatewayClientConfig::for_test(path));
    client.connect_cancellable(|| false).expect("connect");
    client.heartbeat().expect("heartbeat");
    assert_eq!(client.heartbeat_interval(), Duration::from_secs(2));
}

/// A gateway with an invalid server proof must never receive a client proof or
/// any operation on that connection.
#[test]
fn invalid_server_mac_stops_before_client_authentication() {
    let path = socket_path();
    let listener = UnixListener::bind(&path).expect("bind fake gateway");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        let mut reader = path_std_io::BufReader::new(stream.try_clone().expect("clone stream"));
        let mut hello = String::new();
        reader.read_line(&mut hello).expect("read hello");
        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "protocol_version": SOCKET_PROTOCOL_VERSION,
                "ok": true,
                "kind": "challenge",
                "gateway_generation": "44".repeat(32),
                "server_nonce": "55".repeat(32),
                "server_mac": "00".repeat(32),
            })
        )
        .expect("write invalid challenge");
        stream.flush().expect("flush invalid challenge");
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set fixture timeout");
        let mut unexpected = String::new();
        reader
            .read_line(&mut unexpected)
            .expect("read client disconnect");
        assert!(
            unexpected.is_empty(),
            "unexpected client frame: {unexpected}"
        );
    });
    let client = GatewayClient::new(GatewayClientConfig::for_test(path));

    let error = match client.connect_cancellable(|| false) {
        Ok(_) => panic!("invalid server proof must fail"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "Telegram gateway authentication failed");
    server.join().expect("fake gateway");
}

/// Reconnects for one accepted Configure reuse the process generation while
/// creating a fresh per-connection nonce.
#[test]
fn reconnect_reuses_client_generation_with_fresh_nonce() {
    let path = socket_path();
    let listener = UnixListener::bind(&path).expect("bind fake gateway");
    let server = std::thread::spawn(move || {
        let mut hellos = Vec::new();
        for generation in ["44".repeat(32), "55".repeat(32)] {
            let (mut stream, _) = listener.accept().expect("accept client");
            hellos.push(authenticate_test_gateway(&mut stream, &generation));
        }
        hellos
    });
    let client = GatewayClient::new(GatewayClientConfig::for_test(path));

    client.connect_cancellable(|| false).expect("first connect");
    client.disconnect();
    client
        .connect_cancellable(|| false)
        .expect("second connect");
    client.disconnect();
    let hellos = server.join().expect("fake gateway");
    assert_eq!(
        hellos[0]["client_generation"],
        hellos[1]["client_generation"]
    );
    assert_ne!(hellos[0]["client_nonce"], hellos[1]["client_nonce"]);
}

/// A bound Unix listener with a full, unaccepted backlog must return from the
/// connect attempt within the client's finite supervisor cancellation bound.
#[test]
fn full_unaccepted_listener_backlog_connect_is_bounded() {
    let path = socket_path();
    let _listener = UnixListener::bind(&path).expect("bind unaccepted gateway");
    let address = socket2::SockAddr::unix(&path).expect("gateway address");
    let mut fillers = Vec::new();
    let mut saturated = false;
    for _ in 0..16_384 {
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .expect("create filler socket");
        socket.set_nonblocking(true).expect("nonblocking filler");
        match socket.connect(&address) {
            Ok(()) => fillers.push(socket),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                fillers.push(socket);
                saturated = true;
                break;
            }
            Err(error) => panic!("filling Unix listener backlog: {error}"),
        }
    }
    assert!(saturated, "fixture must saturate the Unix accept backlog");

    let client = GatewayClient::new(GatewayClientConfig::for_test(path));
    let started = Instant::now();
    let error = match client.connect_cancellable(|| false) {
        Ok(_) => panic!("cancelled full-backlog connect must fail"),
        Err(error) => error,
    };
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "bounded full-backlog connect took {:?}: {error}",
        started.elapsed()
    );
}

/// A peer that makes byte-by-byte progress cannot reset the response deadline.
#[test]
fn response_reader_enforces_one_absolute_deadline_across_bytes() {
    let (client, mut peer) = UnixStream::pair().expect("socket pair");
    peer.write_all(b"{").expect("write first response byte");
    let started = Instant::now();
    let calls = Cell::new(0);
    let error = read_response_until_with_clock(&client, started + Duration::from_secs(1), || {
        let call = calls.get();
        calls.set(call + 1);
        if call == 0 {
            started
        } else {
            started + Duration::from_secs(2)
        }
    })
    .expect_err("trickle must exceed the absolute deadline");
    assert!(
        error
            .to_string()
            .contains("reading Telegram gateway response")
    );
    assert_eq!(calls.get(), 2);
}
