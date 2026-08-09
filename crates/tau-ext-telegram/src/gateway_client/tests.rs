use std::io as path_std_io;
use std::io::{BufRead, ErrorKind, Write};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

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
    let client = GatewayClient::new(GatewayClientConfig { socket_path: path });
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
                "protocol_version": 0,
                "ok": false,
                "error": "denied",
            })
        )
        .expect("write response");
    });
    let client = GatewayClient::new(GatewayClientConfig { socket_path: path });
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
        let mut line = String::new();
        path_std_io::BufReader::new(stream.try_clone().expect("clone stream"))
            .read_line(&mut line)
            .expect("read request");
        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "protocol_version": 0,
                "ok": true,
                "gateway_generation": "test",
                "heartbeat_interval_seconds": 2,
                "deliveries": [],
            })
        )
        .expect("write response");
    });
    let client = GatewayClient::new(GatewayClientConfig { socket_path: path });
    client.connect_cancellable(|| false).expect("connect");
    assert_eq!(client.heartbeat_interval(), Duration::from_secs(2));
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

    let client = GatewayClient::new(GatewayClientConfig { socket_path: path });
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
