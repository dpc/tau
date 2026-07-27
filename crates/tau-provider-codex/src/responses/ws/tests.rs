mod direct_target_canary;
mod scripted_tcp_server;
mod test_ca;
mod test_server;

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use direct_target_canary::DirectTargetCanary;
use scripted_tcp_server::ScriptedTcpServer;
use test_ca::TestCa;
use test_server::{ServerScript, TestWsServer};

use super::*;
use crate::responses::ResponsesMode;
use crate::{NeverAbort, TurnAbortWaker};

type TestAbortWakerSlot = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync + 'static>>>>;

struct CapturingAbort {
    aborted: Arc<AtomicBool>,
    registered_tx: std_mpsc::Sender<()>,
    waker: TestAbortWakerSlot,
}

impl TurnAbort for CapturingAbort {
    fn is_aborted(&mut self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    fn register_waker(
        &mut self,
        waker: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Box<dyn TurnAbortWaker> {
        *self.waker.lock().expect("waker slot lock") = Some(waker);
        self.registered_tx.send(()).expect("registered receiver");
        Box::new(TestAbortWaker)
    }
}

struct TestAbortWaker;

impl TurnAbortWaker for TestAbortWaker {}

fn test_ws_conn() -> (
    WsConn,
    std_mpsc::Sender<InboundEvent>,
    UnboundedReceiver<WsCommand>,
) {
    let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel();
    let (inbound_tx, inbound_rx) = std_mpsc::channel();
    let runtime = ws_runtime::handle();
    let reader_abort = runtime.spawn(std::future::pending::<()>()).abort_handle();
    let writer_abort = runtime.spawn(std::future::pending::<()>()).abort_handle();
    (
        WsConn {
            outbound_tx,
            inbound_tx: inbound_tx.clone(),
            inbound_rx,
            reader_abort,
            writer_abort,
            opened_at: Instant::now(),
            bearer: "test-token".to_owned(),
            cached_response_anchor: None,
            prewarm_baseline: None,
            carried_response_bytes: 0,
        },
        inbound_tx,
        _outbound_rx,
    )
}

fn test_responses_config() -> ResponsesConfig {
    ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        base_url: "https://chatgpt.com/backend-api".to_owned(),
        api_key: "test-token".to_owned(),
        model_id: "gpt-test".to_owned(),
        raw_context_window: 128_000,
        account_id: None,
        supports_reasoning_effort: true,
        supports_reasoning_summary: true,
        supports_verbosity: true,
        supports_phase: true,
        supports_encrypted_reasoning: true,
        supports_compaction: true,
        supports_prompt_cache_key: true,
    }
}

/// Ensures plain WebSocket traffic uses an HTTP proxy's required absolute-form
/// upgrade without resolving or dialing the target directly.
#[test]
fn websocket_upgrade_uses_selected_http_proxy_absolute_form() {
    use std::io::Write;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("proxy listener");
    let address = listener.local_addr().expect("proxy address");
    let (request_tx, request_rx) = std_mpsc::channel();
    let proxy = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("proxy connection");
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("upgrade request");
            request.push(byte[0]);
            assert!(request.len() < 32 * 1024, "upgrade head is bounded");
        }
        let request_text = String::from_utf8(request).expect("ASCII upgrade");
        let key = request_text
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim())
                })
            })
            .expect("websocket key");
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .expect("upgrade response");
        stream.flush().expect("flush upgrade");
        request_tx.send(request_text).expect("request capture");
    });
    let environment =
        std::collections::BTreeMap::from([("http_proxy".to_owned(), format!("http://{address}"))]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, None);
    let mut config = test_responses_config();
    config.base_url = "http://unresolvable.invalid/backend-api".to_owned();
    let mut abort = NeverAbort;
    let connection = WsConn::connect(&config, "thread-proxy", &network, &mut abort)
        .expect("proxied WebSocket upgrade");
    drop(connection);
    let request = request_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("captured request");
    assert!(
        request.starts_with(
            "GET http://unresolvable.invalid/backend-api/codex/responses HTTP/1.1\r\n"
        ),
        "request was {request:?}",
    );
    proxy.join().expect("proxy thread");
}

/// Ensures a proxy cannot negotiate an extension the client did not request.
#[test]
fn websocket_upgrade_rejects_unsolicited_proxy_extension() {
    use std::io::Write;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("proxy listener");
    let address = listener.local_addr().expect("proxy address");
    let proxy = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("proxy connection");
        let request = read_http_head(&mut stream);
        let key = request
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim())
                })
            })
            .expect("websocket key");
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n"
        )
        .expect("upgrade response");
    });
    let network = tau_provider::OutboundNetworkPolicy::from_environment(
        std::collections::BTreeMap::from([("http_proxy".to_owned(), format!("http://{address}"))]),
        None,
    );
    let mut config = test_responses_config();
    config.base_url = "http://unresolvable.invalid/backend-api".to_owned();
    let mut abort = NeverAbort;
    let Err(LlmError::Outbound(error)) =
        WsConn::connect(&config, "thread-extension", &network, &mut abort)
    else {
        panic!("expected typed proxy protocol error");
    };
    assert_eq!(error.route(), tau_provider::OutboundRouteKind::Proxy);
    assert_eq!(error.phase(), tau_provider::OutboundPhase::Request);
    assert_eq!(error.kind(), tau_provider::OutboundErrorKind::Protocol);
    proxy.join().expect("proxy thread");
}

/// Ensures a direct target cannot negotiate an extension the client did not
/// request.
#[test]
fn websocket_upgrade_rejects_unsolicited_target_extension() {
    use std::io::Write;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("target listener");
    let address = listener.local_addr().expect("target address");
    let target = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("target connection");
        let request = read_http_head(&mut stream);
        let key = request
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim())
                })
            })
            .expect("websocket key");
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n"
        )
        .expect("upgrade response");
    });
    let network = crate::test_network_policy();
    let mut config = test_responses_config();
    config.base_url = format!("http://{address}/backend-api");
    let mut abort = NeverAbort;
    let Err(LlmError::Outbound(error)) =
        WsConn::connect(&config, "thread-extension", &network, &mut abort)
    else {
        panic!("expected typed target protocol error");
    };
    assert_eq!(error.route(), tau_provider::OutboundRouteKind::Direct);
    assert_eq!(error.phase(), tau_provider::OutboundPhase::Request);
    assert_eq!(error.kind(), tau_provider::OutboundErrorKind::Protocol);
    target.join().expect("target thread");
}

/// Ensures a direct target cannot select a WebSocket subprotocol the client did
/// not offer.
#[test]
fn websocket_upgrade_rejects_unsolicited_target_subprotocol() {
    use std::io::Write;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("target listener");
    let address = listener.local_addr().expect("target address");
    let target = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("target connection");
        let request = read_http_head(&mut stream);
        let key = request
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim())
                })
            })
            .expect("websocket key");
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: unoffered\r\n\r\n"
        )
        .expect("upgrade response");
    });
    let network = crate::test_network_policy();
    let mut config = test_responses_config();
    config.base_url = format!("http://{address}/backend-api");
    let mut abort = NeverAbort;
    let Err(LlmError::Outbound(error)) =
        WsConn::connect(&config, "thread-subprotocol", &network, &mut abort)
    else {
        panic!("expected typed target protocol error");
    };
    assert_eq!(error.route(), tau_provider::OutboundRouteKind::Direct);
    assert_eq!(error.phase(), tau_provider::OutboundPhase::Request);
    assert_eq!(error.kind(), tau_provider::OutboundErrorKind::Protocol);
    target.join().expect("target thread");
}

/// Ensures secure WebSocket traffic uses CONNECT, performs target TLS with the
/// additive custom CA, and sends provider credentials only inside the tunnel.
#[test]
fn secure_websocket_proxy_connects_before_target_tls_and_upgrade() {
    use std::io::Write;

    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().expect("CA key");
    let ca = ca_params.self_signed(&ca_key).expect("CA certificate");
    let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
    let leaf = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .expect("leaf params")
        .signed_by(&leaf_key, &ca, &ca_key)
        .expect("leaf certificate");
    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                leaf_key.serialize_der(),
            )),
        )
        .expect("target TLS");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("proxy listener");
    let address = listener.local_addr().expect("proxy address");
    let (capture_tx, capture_rx) = std_mpsc::channel();
    let proxy = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("proxy connection");
        let connect = read_http_head(&mut socket);
        assert!(
            connect.starts_with("CONNECT localhost:4443 HTTP/1.1\r\n"),
            "CONNECT was {connect:?}",
        );
        assert!(
            !connect
                .to_ascii_lowercase()
                .contains("authorization: bearer"),
            "target credential escaped into CONNECT"
        );
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .expect("CONNECT response");
        let connection =
            rustls::ServerConnection::new(Arc::new(tls)).expect("target TLS connection");
        let mut tunnel = rustls::StreamOwned::new(connection, socket);
        let upgrade = read_http_head(&mut tunnel);
        let key = upgrade
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim())
                })
            })
            .expect("websocket key");
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(
            tunnel,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .expect("upgrade response");
        tunnel.flush().expect("flush upgrade");
        capture_tx.send((connect, upgrade)).expect("capture");
    });
    let directory = tempfile::tempdir().expect("CA directory");
    let ca_path = directory.path().join("ca.pem");
    std::fs::write(&ca_path, ca.pem()).expect("write CA");
    let environment =
        std::collections::BTreeMap::from([("https_proxy".to_owned(), format!("http://{address}"))]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, Some(ca_path));
    let mut config = test_responses_config();
    config.base_url = "https://localhost:4443/backend-api".to_owned();
    let mut abort = NeverAbort;
    let connection = WsConn::connect(&config, "thread-connect", &network, &mut abort)
        .expect("tunneled secure WebSocket");
    drop(connection);
    let (_, upgrade) = capture_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("captured tunnel");
    assert!(
        upgrade.starts_with("GET /backend-api/codex/responses HTTP/1.1\r\n"),
        "upgrade was {upgrade:?}",
    );
    assert!(upgrade.contains("authorization: Bearer test-token"));
    proxy.join().expect("proxy thread");
}

/// Ensures WSS through an HTTPS proxy performs proxy TLS, one authenticated
/// CONNECT, target TLS, and WebSocket upgrade without leaking either layer's
/// credentials into the other.
#[test]
fn secure_websocket_through_https_proxy_uses_nested_tls_and_scoped_auth() {
    use std::io::Write;

    let proxy_ca = TestCa::new();
    let target_ca = TestCa::new();
    let proxy_tls = proxy_ca.server_config("localhost");
    let target_tls = target_ca.server_config("localhost");
    let proxy = ScriptedTcpServer::spawn(move |socket| {
        let outer =
            rustls::ServerConnection::new(Arc::new(proxy_tls)).expect("proxy TLS connection");
        let mut outer = rustls::StreamOwned::new(outer, socket);
        let connect = read_http_head(&mut outer);
        outer
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .expect("CONNECT response");
        let inner =
            rustls::ServerConnection::new(Arc::new(target_tls)).expect("target TLS connection");
        let mut inner = rustls::StreamOwned::new(inner, outer);
        let upgrade = read_http_head(&mut inner);
        let key = upgrade
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim())
                })
            })
            .expect("WebSocket key");
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(
            inner,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .expect("upgrade response");
        inner.flush().expect("flush upgrade");
        (connect, upgrade)
    });
    let address = proxy.address();
    let directory = tempfile::tempdir().expect("CA directory");
    let ca_path = directory.path().join("nested-ca.pem");
    std::fs::write(&ca_path, format!("{}\n{}", proxy_ca.pem(), target_ca.pem()))
        .expect("write CA bundle");
    let environment = std::collections::BTreeMap::from([(
        "https_proxy".to_owned(),
        format!("https://proxy-user:proxy-pass@localhost:{}", address.port()),
    )]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, Some(ca_path));
    let mut config = test_responses_config();
    config.base_url = "https://localhost:4444/backend-api".to_owned();
    let mut abort = NeverAbort;
    let connection = WsConn::connect(&config, "thread-nested", &network, &mut abort)
        .expect("nested TLS WebSocket");
    drop(connection);
    let (connect, upgrade) = proxy.finish();
    assert!(connect.starts_with("CONNECT localhost:4444 HTTP/1.1\r\n"));
    assert_eq!(
        connect
            .lines()
            .filter(|line| line
                .to_ascii_lowercase()
                .starts_with("proxy-authorization:"))
            .collect::<Vec<_>>(),
        ["Proxy-Authorization: Basic cHJveHktdXNlcjpwcm94eS1wYXNz"]
    );
    assert!(!connect.contains("test-token"));
    assert!(upgrade.starts_with("GET /backend-api/codex/responses HTTP/1.1\r\n"));
    assert!(upgrade.contains("authorization: Bearer test-token\r\n"));
    assert!(
        !upgrade
            .to_ascii_lowercase()
            .contains("proxy-authorization:")
    );
}

/// Ensures an untrusted HTTPS proxy certificate fails before CONNECT and never
/// reaches the otherwise available direct WSS target.
#[test]
fn wss_proxy_tls_failure_has_no_direct_fallback() {
    let target = DirectTargetCanary::new();
    let proxy_ca = TestCa::new();
    let proxy_tls = proxy_ca.server_config("localhost");
    let proxy = ScriptedTcpServer::spawn(move |socket| {
        let connection =
            rustls::ServerConnection::new(Arc::new(proxy_tls)).expect("proxy TLS connection");
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let mut byte = [0_u8; 1];
        let _ = stream.read(&mut byte);
    });
    let address = proxy.address();
    let environment = std::collections::BTreeMap::from([(
        "https_proxy".to_owned(),
        format!("https://proxy-user:proxy-pass@localhost:{}", address.port()),
    )]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, None);
    let mut config = test_responses_config();
    config.base_url = target.base_url();
    let mut abort = NeverAbort;
    let Err(LlmError::Outbound(error)) =
        WsConn::connect(&config, "thread-proxy-tls", &network, &mut abort)
    else {
        panic!("expected typed HTTPS proxy TLS failure");
    };
    assert_eq!(error.route(), tau_provider::OutboundRouteKind::Proxy);
    assert_eq!(error.phase(), tau_provider::OutboundPhase::Proxy);
    assert_eq!(error.kind(), tau_provider::OutboundErrorKind::Transport);
    let projection = format!("{error:?} {error}");
    for canary in ["proxy-user", "proxy-pass", "localhost", "test-token"] {
        assert!(
            !projection.contains(canary),
            "leaked {canary}: {projection}"
        );
    }
    proxy.finish();
    target.assert_untouched();
}

/// Ensures a hidden CONNECT rejection remains generic Proxy/Transport and does
/// not trigger a direct WSS fallback, preserving the approved reqwest boundary.
#[test]
fn wss_connect_rejection_is_generic_and_has_no_direct_fallback() {
    use std::io::Write;

    let target = DirectTargetCanary::new();
    let authority = target.authority();
    let proxy = ScriptedTcpServer::spawn(move |mut stream| {
        let connect = read_http_head(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .expect("CONNECT rejection");
        connect
    });
    let address = proxy.address();
    let environment = std::collections::BTreeMap::from([(
        "https_proxy".to_owned(),
        format!("http://proxy-user:proxy-pass@{address}"),
    )]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, None);
    let mut config = test_responses_config();
    config.base_url = target.base_url();
    let mut abort = NeverAbort;
    let Err(LlmError::Outbound(error)) =
        WsConn::connect(&config, "thread-connect-reject", &network, &mut abort)
    else {
        panic!("expected hidden CONNECT rejection");
    };
    assert_eq!(error.route(), tau_provider::OutboundRouteKind::Proxy);
    assert_eq!(error.phase(), tau_provider::OutboundPhase::Proxy);
    assert_eq!(error.kind(), tau_provider::OutboundErrorKind::Transport);
    let connect = proxy.finish();
    assert!(connect.starts_with(&format!("CONNECT {authority} HTTP/1.1\r\n")));
    assert_eq!(
        connect
            .lines()
            .filter(|line| line
                .to_ascii_lowercase()
                .starts_with("proxy-authorization:"))
            .collect::<Vec<_>>(),
        ["Proxy-Authorization: Basic cHJveHktdXNlcjpwcm94eS1wYXNz"]
    );
    assert!(!connect.contains("test-token"));
    target.assert_untouched();
}

/// Ensures inner target TLS rejection after a successful CONNECT remains a
/// redacted generic proxy transport failure and cannot fall back direct.
#[test]
fn wss_target_tls_failure_has_no_direct_fallback() {
    use std::io::Write;

    let target = DirectTargetCanary::new();
    let authority = target.authority();
    let target_ca = TestCa::new();
    let target_tls = target_ca.server_config("localhost");
    let proxy = ScriptedTcpServer::spawn(move |mut socket| {
        let connect = read_http_head(&mut socket);
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .expect("CONNECT response");
        let connection =
            rustls::ServerConnection::new(Arc::new(target_tls)).expect("target TLS connection");
        let mut tunnel = rustls::StreamOwned::new(connection, socket);
        let mut byte = [0_u8; 1];
        let _ = tunnel.read(&mut byte);
        connect
    });
    let address = proxy.address();
    let environment =
        std::collections::BTreeMap::from([("https_proxy".to_owned(), format!("http://{address}"))]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, None);
    let mut config = test_responses_config();
    config.base_url = target.base_url();
    let mut abort = NeverAbort;
    let Err(LlmError::Outbound(error)) =
        WsConn::connect(&config, "thread-target-tls", &network, &mut abort)
    else {
        panic!("expected tunneled target TLS failure");
    };
    assert_eq!(error.route(), tau_provider::OutboundRouteKind::Proxy);
    assert_eq!(error.phase(), tau_provider::OutboundPhase::Proxy);
    assert_eq!(error.kind(), tau_provider::OutboundErrorKind::Transport);
    let projection = format!("{error:?} {error}");
    assert!(!projection.contains("localhost"));
    assert!(!projection.contains("test-token"));
    let connect = proxy.finish();
    assert!(connect.starts_with(&format!("CONNECT {authority} HTTP/1.1\r\n")));
    target.assert_untouched();
}

/// Ensures a target-authored WebSocket upgrade failure after successful CONNECT
/// never causes a direct transport fallback.
#[test]
fn wss_upgrade_failure_has_no_direct_fallback() {
    use std::io::Write;

    let target = DirectTargetCanary::new();
    let target_ca = TestCa::new();
    let target_tls = target_ca.server_config("localhost");
    let proxy = ScriptedTcpServer::spawn(move |mut socket| {
        let _connect = read_http_head(&mut socket);
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .expect("CONNECT response");
        let connection =
            rustls::ServerConnection::new(Arc::new(target_tls)).expect("target TLS connection");
        let mut tunnel = rustls::StreamOwned::new(connection, socket);
        let upgrade = read_http_head(&mut tunnel);
        tunnel
            .write_all(
                b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .expect("upgrade rejection");
        upgrade
    });
    let address = proxy.address();
    let directory = tempfile::tempdir().expect("CA directory");
    let ca_path = directory.path().join("target-ca.pem");
    std::fs::write(&ca_path, target_ca.pem()).expect("write target CA");
    let environment =
        std::collections::BTreeMap::from([("https_proxy".to_owned(), format!("http://{address}"))]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, Some(ca_path));
    let mut config = test_responses_config();
    config.base_url = target.base_url();
    let mut abort = NeverAbort;
    assert!(matches!(
        WsConn::connect(&config, "thread-upgrade-fail", &network, &mut abort),
        Err(LlmError::WsUpgradeRequired)
    ));
    let upgrade = proxy.finish();
    assert!(upgrade.starts_with("GET /backend-api/codex/responses HTTP/1.1\r\n"));
    assert!(upgrade.contains("authorization: Bearer test-token\r\n"));
    target.assert_untouched();
}

fn read_http_head(stream: &mut impl Read) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("HTTP head");
        head.push(byte[0]);
        assert!(head.len() < 32 * 1024, "HTTP head is bounded");
    }
    String::from_utf8(head).expect("ASCII HTTP head")
}

/// Mutable credential/account header failures retry as auth/config and a
/// corrected profile builds successfully on the next attempt.
#[test]
fn mutable_ws_header_configuration_can_be_repaired() {
    for invalid_account in [false, true] {
        let mut config = test_responses_config();
        if invalid_account {
            config.account_id = Some("bad\naccount".to_owned());
        } else {
            config.api_key = "bad\ntoken".to_owned();
        }
        let error = build_request(&config, "thread-id").expect_err("invalid mutable header");
        assert!(matches!(error, LlmError::ReloadableConfig(_)));
        assert_eq!(
            error.retry_decision().map(|decision| decision.class),
            Some(tau_provider::retry_policy::RetryClass::Auth)
        );

        config.api_key = "repaired-token".to_owned();
        config.account_id = Some("repaired-account".to_owned());
        build_request(&config, "thread-id").expect("repaired profile request");
    }
}

/// Unsupported configured schemes remain retryable because profile reload can
/// repair the endpoint before a later attempt.
#[test]
fn unsupported_ws_scheme_is_reloadable() {
    let mut config = test_responses_config();
    config.base_url = "file:///tmp/provider".to_owned();
    let error = build_request(&config, "thread-id").expect_err("unsupported WS scheme");
    assert!(matches!(error, LlmError::ReloadableConfig(_)));
    assert_eq!(
        error.retry_decision().map(|decision| decision.class),
        Some(tau_provider::retry_policy::RetryClass::Auth)
    );
    config.base_url = "https://chatgpt.com/backend-api".to_owned();
    build_request(&config, "thread-id").expect("repaired WS URL");
}

/// A fresh WebSocket upgrade that never resolves must return a retryable,
/// content-free transport timeout instead of holding the prompt forever.
#[test]
fn ws_connect_wait_is_bounded() {
    let mut abort = NeverAbort;
    let result = wait_for_connect(
        &ws_runtime::handle(),
        &mut abort,
        Duration::from_millis(20),
        std::future::pending::<Result<(), ()>>(),
    );
    assert!(matches!(result, Err(ConnectWaitError::Timeout)));
    let network = crate::test_network_policy();
    let error = map_connect_wait_error(
        ConnectWaitError::Timeout,
        &network,
        "wss://target.example/codex/responses",
    );
    let LlmError::Outbound(outbound) = &error else {
        panic!("expected typed deadline");
    };
    assert_eq!(outbound.route(), tau_provider::OutboundRouteKind::Direct);
    assert_eq!(outbound.phase(), tau_provider::OutboundPhase::Connect);
    assert_eq!(outbound.kind(), tau_provider::OutboundErrorKind::Deadline);
    assert_eq!(
        error.retry_decision().map(|decision| decision.class),
        Some(tau_provider::retry_policy::RetryClass::Transport)
    );
}

/// Cancellation must wake a fresh WebSocket upgrade before its connection
/// deadline, matching the cooperative wake contract used by active turns.
#[test]
fn ws_connect_wait_is_cancellation_aware() {
    let aborted = Arc::new(AtomicBool::new(false));
    let (registered_tx, registered_rx) = std_mpsc::channel();
    let waker = Arc::new(Mutex::new(None));
    let mut abort = CapturingAbort {
        aborted: Arc::clone(&aborted),
        registered_tx,
        waker: Arc::clone(&waker),
    };
    let (result_tx, result_rx) = std_mpsc::channel();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            let result = wait_for_connect(
                &ws_runtime::handle(),
                &mut abort,
                Duration::from_secs(30),
                std::future::pending::<Result<(), ()>>(),
            );
            result_tx.send(result).expect("result receiver");
        });
        registered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("connect abort waker registration");

        aborted.store(true, Ordering::SeqCst);
        waker
            .lock()
            .expect("waker slot lock")
            .as_ref()
            .expect("registered connect waker")
            .clone()();
        assert!(matches!(
            result_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("connect cancellation result"),
            Err(ConnectWaitError::Canceled)
        ));
    });
}

/// An abort source may become canceled while registering without invoking a
/// late waker; the mandatory post-registration check must still win
/// immediately.
#[test]
fn ws_connect_rechecks_cancellation_after_waker_registration() {
    struct AbortDuringRegistration(bool);
    impl TurnAbort for AbortDuringRegistration {
        fn is_aborted(&mut self) -> bool {
            self.0
        }

        fn register_waker(
            &mut self,
            _waker: Arc<dyn Fn() + Send + Sync + 'static>,
        ) -> Box<dyn TurnAbortWaker> {
            self.0 = true;
            Box::new(TestAbortWaker)
        }
    }

    let mut abort = AbortDuringRegistration(false);
    let result = wait_for_connect(
        &ws_runtime::handle(),
        &mut abort,
        Duration::from_secs(30),
        std::future::pending::<Result<(), ()>>(),
    );
    assert!(matches!(result, Err(ConnectWaitError::Canceled)));
    assert!(matches!(
        map_connect_wait_error(
            ConnectWaitError::Canceled,
            &crate::test_network_policy(),
            "wss://target.example/codex/responses",
        ),
        LlmError::Canceled
    ));
}

struct PromptFixture {
    context: tau_proto::PromptContext,
    session_id: tau_proto::SessionId,
    agent_id: tau_proto::AgentId,
    originator: tau_proto::PromptOriginator,
}

impl PromptFixture {
    fn new() -> Self {
        Self {
            context: tau_proto::PromptContext::default(),
            session_id: tau_proto::SessionId::parse("session-test")
                .expect("known-safe SessionId must be valid"),
            agent_id: tau_proto::AgentId::parse("agent-test").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }
    }

    fn payload(&self) -> PromptPayload<'_> {
        PromptPayload {
            system_prompt: "",
            context: &self.context,
            tools: &[],
            params: tau_proto::ModelParams::default(),
            tool_choice: tau_proto::ToolChoice::default(),
            compaction: None,
            originator: &self.originator,
            share_user_cache_key: false,
            session_id: &self.session_id,
            agent_id: &self.agent_id,
            debug_provider_requests: false,
        }
    }
}

/// A localhost WebSocket round trip must use the production upgrade, request
/// lowering, reader task, frame parser, and typed stream-state accumulation.
#[test]
fn localhost_ws_round_trip_lowers_and_parses_production_frames() {
    let frames = [
        serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "hello",
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {
                "type": "function_call",
                "id": "fc_local",
                "call_id": "call_local",
                "name": "shell",
                "status": "in_progress",
            },
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 1,
            "delta": "{\"command\":\"pwd\"}",
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_local",
                "usage": {
                    "input_tokens": 11,
                    "output_tokens": 7,
                    "input_tokens_details": { "cached_tokens": 3 },
                },
            },
        }),
    ]
    .map(|frame| frame.to_string())
    .to_vec();
    let server = TestWsServer::spawn(ServerScript::Frames(frames));
    let mut config = test_responses_config();
    config.base_url = server.base_url();
    config.account_id = Some("account-local".to_owned());
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let mut abort = NeverAbort;
    let mut conn = WsConn::connect(
        &config,
        "thread-local",
        &crate::test_network_policy(),
        &mut abort,
    )
    .expect("connect localhost WebSocket");

    let result = conn
        .run_turn(
            &config,
            "ap-local-round-trip",
            &request,
            None,
            &mut abort,
            &mut |_| {},
            &mut |_| {},
        )
        .expect("complete localhost WebSocket turn");
    server.wait_for_request();
    let capture = server.capture();
    let capture = capture.lock().expect("localhost WebSocket capture");
    assert_eq!(capture.requests.len(), 1);
    let envelope: serde_json::Value =
        serde_json::from_str(&capture.requests[0]).expect("production request envelope");
    assert_eq!(envelope["type"], "response.create");
    assert_eq!(envelope["model"], "gpt-test");
    assert_eq!(
        capture.headers.get("thread-id").map(String::as_str),
        Some("thread-local")
    );
    assert_eq!(
        capture.headers.get("session-id").map(String::as_str),
        Some("thread-local")
    );
    assert_eq!(
        capture
            .headers
            .get("chatgpt-account-id")
            .map(String::as_str),
        Some("account-local")
    );
    drop(capture);

    assert_eq!(result.state.text, "hello");
    assert_eq!(result.state.response_id.as_deref(), Some("resp_local"));
    assert_eq!(result.state.input_tokens, Some(11));
    assert_eq!(result.state.cached_tokens, Some(3));
    assert_eq!(result.state.output_tokens, Some(7));
    let output = result.state.into_output_items();
    assert_eq!(output.len(), 2);
    let tau_proto::ContextItem::ToolCall(call) = &output[1] else {
        panic!("expected parsed function call");
    };
    assert_eq!(call.call_id.as_str(), "call_local");
    assert_eq!(call.name.as_str(), "shell");
    assert_eq!(
        call.raw_arguments_json.as_deref(),
        Some("{\"command\":\"pwd\"}")
    );

    drop(conn);
    server.join();
}

/// A silent upgraded localhost peer must be interrupted through the production
/// connection's abort-waker path and return typed cancellation.
#[test]
fn localhost_ws_silent_turn_returns_typed_cancellation() {
    let server = TestWsServer::spawn(ServerScript::Silent);
    let mut config = test_responses_config();
    config.base_url = server.base_url();
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let mut connect_abort = NeverAbort;
    let mut conn = WsConn::connect(
        &config,
        "thread-cancel",
        &crate::test_network_policy(),
        &mut connect_abort,
    )
    .expect("connect localhost WS");
    let aborted = Arc::new(AtomicBool::new(false));
    let (registered_tx, registered_rx) = std_mpsc::channel();
    let waker = Arc::new(Mutex::new(None));
    let mut abort = CapturingAbort {
        aborted: Arc::clone(&aborted),
        registered_tx,
        waker: Arc::clone(&waker),
    };
    let (result_tx, result_rx) = std_mpsc::channel();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            let result = conn.run_turn(
                &config,
                "ap-local-cancel",
                &request,
                None,
                &mut abort,
                &mut |_| {},
                &mut |_| {},
            );
            result_tx
                .send(result)
                .expect("cancellation result receiver");
        });
        server.wait_for_request();
        registered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("turn abort-waker registration");
        aborted.store(true, Ordering::SeqCst);
        waker
            .lock()
            .expect("waker slot lock")
            .as_ref()
            .expect("registered turn waker")();
        assert!(matches!(
            result_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("typed cancellation result"),
            Err(LlmError::Canceled)
        ));
    });

    drop(conn);
    server.join();
}

/// A silent upgraded localhost peer must traverse the production reader and
/// request writer before the test-sized provider-frame idle deadline fires.
#[test]
fn localhost_ws_silent_turn_returns_typed_idle_timeout() {
    let server = TestWsServer::spawn(ServerScript::Silent);
    let mut config = test_responses_config();
    config.base_url = server.base_url();
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let envelope = build_ws_envelope(&config, &request, None, None);
    let mut abort = NeverAbort;
    let mut conn = WsConn::connect(
        &config,
        "thread-timeout",
        &crate::test_network_policy(),
        &mut abort,
    )
    .expect("connect localhost WS");

    let result = conn.run_envelope_with_timeouts(
        "ap-local-timeout",
        envelope,
        None,
        &mut abort,
        EnvelopeTimeouts {
            idle: Duration::from_millis(20),
            absolute: None,
        },
        &mut |_| {},
        &mut |_| {},
    );
    server.wait_for_request();
    let Err(LlmError::HttpStatus(0, body)) = result else {
        panic!("expected typed provider-frame idle timeout");
    };
    assert!(body.contains("provider stream idle timeout"), "{body}");
    assert!(body.contains("transport=Websocket"), "{body}");
    assert!(body.contains("agent_prompt_id=ap-local-timeout"), "{body}");
    assert!(body.contains("partial_output=false"), "{body}");
    let error = LlmError::HttpStatus(0, body);
    assert_eq!(
        error.retry_decision().map(|decision| decision.class),
        Some(tau_provider::retry_policy::RetryClass::Transport)
    );

    drop(conn);
    server.join();
}

/// Ensure WebSocket turns wake promptly from registered cancellation rather
/// than waiting for the five-minute provider-stream idle timeout.
#[test]
fn ws_turn_abort_waker_returns_typed_cancellation_promptly() {
    let (mut conn, _inbound_tx, _outbound_rx) = test_ws_conn();
    let config = test_responses_config();
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let aborted = Arc::new(AtomicBool::new(false));
    let (registered_tx, registered_rx) = std_mpsc::channel();
    let waker = Arc::new(Mutex::new(None));
    let mut abort = CapturingAbort {
        aborted: Arc::clone(&aborted),
        registered_tx,
        waker: Arc::clone(&waker),
    };
    let (result_tx, result_rx) = std_mpsc::channel();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            let result = conn.run_turn(
                &config,
                "ap-ws-abort",
                &request,
                None,
                &mut abort,
                &mut |_| {},
                &mut |_| {},
            );
            result_tx.send(result).expect("result receiver");
        });
        registered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("abort waker registration");

        let start = Instant::now();
        aborted.store(true, Ordering::SeqCst);
        let wake = waker
            .lock()
            .expect("waker slot lock")
            .as_ref()
            .expect("registered waker")
            .clone();
        wake();

        let result = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("prompt cancellation result");
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(matches!(result, Err(LlmError::Canceled)));
    });
}

/// Regression for tau-agent-jx2z: a WebSocket turn that never receives a
/// terminal provider frame must trip the per-turn idle watchdog instead of
/// waiting forever on a quiet pooled socket.
#[test]
fn ws_turn_returns_idle_timeout_error_after_stalled_frame_stream() {
    let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
    let config = test_responses_config();
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let envelope = build_ws_envelope(&config, &request, None, None);
    let mut abort = NeverAbort;

    inbound_tx
        .send(InboundEvent::Event {
            text: r#"{"type":"response.output_text.delta","delta":"hello"}"#.into(),
        })
        .expect("queue partial WS frame");
    let result = conn.run_envelope_with_timeouts(
        "ap-stalled-ws",
        envelope,
        None,
        &mut abort,
        EnvelopeTimeouts {
            idle: Duration::from_millis(50),
            absolute: None,
        },
        &mut |_| {},
        &mut |_| {},
    );

    let Err(LlmError::HttpStatus(0, body)) = result else {
        panic!("expected timeout stream error");
    };
    assert!(body.contains("provider stream idle timeout"), "{body}");
    assert!(body.contains("transport=Websocket"), "{body}");
    assert!(body.contains("agent_prompt_id=ap-stalled-ws"), "{body}");
    assert!(body.contains("elapsed="), "{body}");
    assert!(body.contains("idle="), "{body}");
    assert!(body.contains("idle_timeout="), "{body}");
    assert!(body.contains("partial_output=true"), "{body}");
}

/// Prewarm's elapsed absolute deadline wins even when nonterminal provider
/// frames are already queued for processing.
#[test]
fn prewarm_absolute_timeout_preempts_queued_nonterminal_frames() {
    let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
    let config = test_responses_config();
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let envelope = build_ws_envelope(&config, &request, None, Some(false));
    let mut abort = NeverAbort;
    for _ in 0..4 {
        inbound_tx
            .send(InboundEvent::Event {
                text: r#"{"type":"response.output_text.delta","delta":"x"}"#.into(),
            })
            .expect("queue nonterminal frame");
    }

    let result = conn.run_envelope_with_timeouts(
        "<prewarm>",
        envelope,
        None,
        &mut abort,
        EnvelopeTimeouts {
            idle: Duration::from_secs(1),
            absolute: Some(Duration::ZERO),
        },
        &mut |_| {},
        &mut |_| {},
    );

    assert!(matches!(
        result,
        Err(LlmError::HttpStatus(0, body))
            if body == "websocket prewarm response timeout"
    ));
}

/// Rejected provider data frames still contribute to cumulative transport bytes
/// before the socket is retired.
#[test]
fn malformed_text_frame_counts_bytes_before_protocol_error() {
    let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
    let config = test_responses_config();
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let malformed = "{not-json";
    inbound_tx
        .send(InboundEvent::Error {
            detail: "malformed JSON text frame".to_owned(),
            response_bytes: malformed.len(),
        })
        .expect("queue malformed frame");
    let mut observed_bytes = 0;
    let error = match conn.run_turn(
        &config,
        "ap-malformed",
        &request,
        None,
        &mut NeverAbort,
        &mut |_| {},
        &mut |state| observed_bytes = state.response_bytes_received(),
    ) {
        Ok(_) => panic!("malformed frame must retire socket"),
        Err(error) => error,
    };
    assert!(matches!(error, LlmError::HttpStatus(0, _)));
    assert_eq!(observed_bytes, malformed.len() as u64);
}

/// Quota parsing is mode-independent: standard and Lite WebSocket turns both
/// surface the official nameless default-pool event.
#[test]
fn ws_turn_surfaces_nameless_default_quota_in_both_modes() {
    for (model_id, mode) in [
        ("gpt-test", ResponsesMode::Standard),
        ("gpt-5.6-sol", ResponsesMode::LiteCompatibility),
    ] {
        let (mut conn, inbound_tx, mut outbound_rx) = test_ws_conn();
        let mut config = test_responses_config();
        config.model_id = model_id.to_owned();
        config.mode = mode;
        let fixture = PromptFixture::new();
        let request = fixture.payload();
        let mut abort = NeverAbort;
        let mut observed_limit = None;

        for text in [
            r#"{"type":"codex.rate_limits","plan_type":"plus","rate_limits":{"secondary":{"used_percent":45,"window_minutes":10080,"reset_at":1700600000}}}"#,
            r#"{"type":"response.completed","response":{"id":"resp_quota"}}"#,
        ] {
            inbound_tx
                .send(InboundEvent::Event { text: text.into() })
                .expect("queue WS fixture frame");
        }

        conn.run_turn(
            &config,
            "ap-ws-quota",
            &request,
            None,
            &mut abort,
            &mut |_| {},
            &mut |state| {
                if let Some(observation) = state.quota_observation.as_ref() {
                    observed_limit = observation
                        .active_limit_id
                        .as_ref()
                        .map(ToString::to_string);
                }
            },
        )
        .expect("completed WS turn");
        assert_eq!(observed_limit.as_deref(), Some("codex"), "{model_id}");
        let WsCommand::SendText(request_text) =
            outbound_rx.try_recv().expect("sent WS request envelope");
        let request_json: serde_json::Value =
            serde_json::from_str(&request_text).expect("valid WS request envelope");
        let lite_marker = request_json
            .pointer("/client_metadata/ws_request_header_x_openai_internal_codex_responses_lite")
            .and_then(serde_json::Value::as_str);
        assert_eq!(
            lite_marker,
            mode.is_lite_compatibility().then_some("true"),
            "{model_id}"
        );
    }
}
