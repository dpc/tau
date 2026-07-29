mod fixture;

use fixture::{
    DirectTargetCanary, FailingProxyResolver, ScriptedTcpServer, TestCa, read_http_head,
};

use super::*;

fn policy(entries: &[(&str, &str)]) -> OutboundNetworkPolicy {
    OutboundNetworkPolicy::from_environment(
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        None,
    )
}

/// Ensures scheme-specific proxy selection and ALL_PROXY fallback are stable.
#[test]
fn route_selection_is_scheme_specific() {
    let selected_policy = policy(&[
        ("http_proxy", "http://http-proxy.example:8080"),
        ("HTTPS_PROXY", "https://secure-proxy.example:8443"),
    ]);
    assert_eq!(
        selected_policy
            .route_kind("http://target.example")
            .expect("route selection"),
        OutboundRouteKind::Proxy
    );
    assert_eq!(
        selected_policy
            .route_kind("wss://target.example")
            .expect("route selection"),
        OutboundRouteKind::Proxy
    );
    let fallback = policy(&[("ALL_PROXY", "http://fallback-proxy.example:8080")]);
    for target in ["http://target.example", "https://target.example"] {
        assert_eq!(
            fallback.route_kind(target).expect("ALL_PROXY route"),
            OutboundRouteKind::Proxy,
            "{target}"
        );
    }
}

/// Ensures lowercase variables win conflicts and malformed selected values fail
/// closed.
#[test]
fn proxy_precedence_does_not_silently_fallback() {
    let policy = policy(&[
        ("https_proxy", "socks5://secret.example"),
        ("HTTPS_PROXY", "http://valid.example"),
    ]);
    let error = policy
        .client_for("https://target.example")
        .expect_err("selected malformed proxy must fail");
    assert_eq!(error.phase(), OutboundPhase::Configure);
}

/// Ensures the policy owns its startup snapshot rather than observing later
/// mutations of the caller's environment map.
#[test]
fn policy_is_immutable_after_startup_capture() {
    use std::io::Write;

    let first_proxy = ScriptedTcpServer::spawn(|mut stream| {
        let request = read_http_head(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("captured proxy response");
        request
    });
    let replacement_proxy = DirectTargetCanary::new();
    let mut environment = BTreeMap::from([(
        "http_proxy".to_owned(),
        format!("http://{}", first_proxy.address()),
    )]);
    let policy = OutboundNetworkPolicy::from_environment(environment.clone(), None);
    environment.insert(
        "http_proxy".to_owned(),
        format!("http://{}", replacement_proxy.address()),
    );
    let url = "http://unresolvable.invalid/startup-snapshot";
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let response = runtime
        .block_on(policy.client_for(url).expect("client").get(url).send())
        .expect("captured proxy response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let request = first_proxy.finish();
    assert!(
        request.starts_with("GET http://unresolvable.invalid/startup-snapshot HTTP/1.1\r\n"),
        "request was {request:?}"
    );
    replacement_proxy.assert_untouched();
}

/// Ensures a proxy-auth response is represented by closed typed facts without
/// retaining endpoints or credentials.
#[test]
fn proxy_authentication_is_typed_and_redacted() {
    let proxy = "http://canary-user:canary-password@canary-proxy.example";
    let policy = policy(&[("http_proxy", proxy), ("https_proxy", proxy)]);
    let error = policy
        .proxy_response_error("http://canary-target.example", 407)
        .expect("proxy-auth classification");
    assert_eq!(error.route(), OutboundRouteKind::Proxy);
    assert_eq!(error.phase(), OutboundPhase::Proxy);
    assert_eq!(error.kind(), OutboundErrorKind::ProxyAuthentication);
    let projection = format!("{error:?} {error}");
    for canary in [
        "canary-user",
        "canary-password",
        "canary-proxy",
        "canary-target",
    ] {
        assert!(
            !projection.contains(canary),
            "leaked {canary}: {projection}"
        );
    }
    for target in [
        "https://canary-target.example",
        "wss://canary-target.example/socket",
    ] {
        assert_eq!(
            policy.route_kind(target).expect("secure proxy route"),
            OutboundRouteKind::Proxy
        );
        assert!(
            policy.proxy_response_error(target, 407).is_none(),
            "visible secure-target 407 must remain target-authored: {target}"
        );
    }
}

/// Ensures an unrelated non-UTF-8 environment value cannot crash or poison the
/// named provider-network snapshot.
#[cfg(unix)]
#[test]
fn unrelated_non_utf8_environment_is_ignored() {
    use std::os::unix::ffi::OsStringExt;

    const CHILD: &str = "TAU_TEST_NON_UTF8_CHILD";
    if std::env::var_os(CHILD).is_some() {
        assert_eq!(
            OutboundNetworkPolicy::from_env()
                .route_kind("http://localhost")
                .expect("unrelated value must not poison the policy"),
            OutboundRouteKind::Direct
        );
        return;
    }
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command
        .arg("--exact")
        .arg("outbound_network::tests::unrelated_non_utf8_environment_is_ignored")
        .arg("--nocapture")
        .env(CHILD, "1")
        .env(
            "TAU_TEST_UNRELATED_NON_UTF8",
            std::ffi::OsString::from_vec(vec![0xff]),
        );
    for key in [
        "http_proxy",
        "HTTP_PROXY",
        "https_proxy",
        "HTTPS_PROXY",
        "all_proxy",
        "ALL_PROXY",
        "no_proxy",
        "NO_PROXY",
        "TAU_PROVIDER_CA_BUNDLE",
    ] {
        command.env_remove(key);
    }
    assert!(command.status().expect("child test").success());
}

/// Ensures NO_PROXY matching uses label boundaries and exact optional ports.
#[test]
fn no_proxy_matching_is_dns_free_and_port_aware() {
    let policy = policy(&[
        ("HTTPS_PROXY", "http://proxy.example"),
        ("NO_PROXY", ".internal.example,localhost:8443,10.0.0.0/8"),
    ]);
    assert_eq!(
        policy
            .route_kind("https://api.internal.example")
            .expect("route selection"),
        OutboundRouteKind::Direct
    );
    assert_eq!(
        policy
            .route_kind("https://notinternal.example")
            .expect("route selection"),
        OutboundRouteKind::Proxy
    );
    assert_eq!(
        policy
            .route_kind("https://localhost:8443")
            .expect("route selection"),
        OutboundRouteKind::Direct
    );
    assert_eq!(
        policy
            .route_kind("https://localhost")
            .expect("route selection"),
        OutboundRouteKind::Proxy
    );
    assert_eq!(
        policy
            .route_kind("https://10.2.3.4")
            .expect("route selection"),
        OutboundRouteKind::Direct
    );
}

/// Ensures proxy credentials are decoded once and never exposed by safe errors
/// or debug output.
#[test]
fn credentials_are_decoded_and_redacted() {
    let policy = policy(&[("HTTPS_PROXY", "http://user%40corp:s3cr%25t@proxy.example")]);
    let debug = format!("{policy:?}");
    assert!(!debug.contains("user"));
    assert!(!debug.contains("s3cr"));
    assert!(policy.client_for("https://target.example").is_ok());
}

/// Ensures malformed NO_PROXY state rejects every route instead of bypassing
/// the proxy.
#[test]
fn malformed_no_proxy_fails_closed() {
    let policy = policy(&[
        ("HTTPS_PROXY", "http://proxy.example"),
        ("NO_PROXY", "bad host"),
    ]);
    assert!(policy.client_for("https://target.example").is_err());
}

/// Ensures an HTTP proxy receives absolute-form requests and decoded Basic
/// credentials while the unresolvable target is never dialed directly.
#[test]
fn http_proxy_wire_uses_absolute_form_and_decoded_credentials() {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind proxy");
    let address = listener.local_addr().expect("proxy address");
    let proxy = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("proxy connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("proxy request");
            request.push(byte[0]);
            assert!(request.len() < 16 * 1024, "proxy request head is bounded");
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .expect("proxy response");
        String::from_utf8(request).expect("ASCII request")
    });
    let policy = policy(&[(
        "http_proxy",
        &format!("http://user%40corp:s3cr%25t@{address}"),
    )]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let response = runtime
        .block_on(
            policy
                .client_for("http://unresolvable.invalid/resource")
                .expect("client")
                .get("http://unresolvable.invalid/resource")
                .send(),
        )
        .expect("proxied response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let request = proxy.join().expect("proxy thread");
    assert!(request.starts_with("GET http://unresolvable.invalid/resource HTTP/1.1\r\n"));
    assert!(
        request.contains("proxy-authorization: Basic dXNlckBjb3JwOnMzY3IldA==\r\n",),
        "request was {request:?}",
    );
}

/// Ensures the platform verifier accepts an additive custom CA for target TLS
/// without introducing a certificate-verification disable path.
#[test]
fn custom_ca_is_additive_for_https_target() {
    use std::io::{Read, Write};

    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().expect("CA key");
    let ca = ca_params.self_signed(&ca_key).expect("CA certificate");
    let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
    let leaf = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .expect("leaf params")
        .signed_by(&leaf_key, &ca, &ca_key)
        .expect("leaf certificate");
    let server = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                leaf_key.serialize_der(),
            )),
        )
        .expect("server TLS");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("TLS listener");
    let address = listener.local_addr().expect("TLS address");
    let server = std::thread::spawn(move || {
        let (socket, _) = listener.accept().expect("TLS connection");
        let connection =
            rustls::ServerConnection::new(Arc::new(server)).expect("server connection");
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("HTTPS request");
            request.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .expect("HTTPS response");
    });
    let directory = tempfile::tempdir().expect("CA directory");
    let path = directory.path().join("provider-ca.pem");
    std::fs::write(&path, ca.pem()).expect("write CA");
    let policy = OutboundNetworkPolicy::from_environment(BTreeMap::new(), Some(path));
    let url = format!("https://localhost:{}/", address.port());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let response = runtime
        .block_on(policy.client_for(&url).expect("client").get(&url).send())
        .expect("custom CA response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    server.join().expect("TLS server");
}

/// Ensures CA files are consumed at startup: deleting the source bundle after
/// policy construction cannot change the prepared TLS verifier.
#[test]
fn custom_ca_file_is_immutable_after_startup_capture() {
    use std::io::{Read, Write};

    let ca = TestCa::new();
    let server_tls = ca.server_config("localhost");
    let server = ScriptedTcpServer::spawn(move |socket| {
        let connection =
            rustls::ServerConnection::new(Arc::new(server_tls)).expect("server connection");
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let mut request = [0_u8; 8192];
        assert!(stream.read(&mut request).expect("HTTPS request") > 0);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("HTTPS response");
    });
    let address = server.address();
    let directory = tempfile::tempdir().expect("CA directory");
    let path = directory.path().join("startup-ca.pem");
    std::fs::write(&path, ca.pem()).expect("write CA");
    let policy = OutboundNetworkPolicy::from_environment(BTreeMap::new(), Some(path.clone()));
    std::fs::remove_file(path).expect("remove captured CA file");
    let url = format!("https://localhost:{}/", address.port());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let response = runtime
        .block_on(policy.client_for(&url).expect("client").get(&url).send())
        .expect("captured CA remains effective");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    server.finish();
}

/// Ensures an untrusted direct target certificate fails as typed target TLS
/// without retaining the endpoint or certificate diagnostics.
#[test]
fn untrusted_direct_target_tls_is_redacted() {
    use std::io::Read;

    let ca = TestCa::new();
    let server_tls = ca.server_config("localhost");
    let server = ScriptedTcpServer::spawn(move |socket| {
        let connection =
            rustls::ServerConnection::new(Arc::new(server_tls)).expect("server connection");
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let mut byte = [0_u8; 1];
        // This call is intentionally best-effort; preserve the existing discarded
        // result. ast-grep-ignore: let-underscore-call
        let _ = stream.read(&mut byte);
    });
    let address = server.address();
    let policy = policy(&[]);
    let url = format!("https://localhost:{}/target-tls-canary", address.port());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let raw = runtime
        .block_on(policy.client_for(&url).expect("client").get(&url).send())
        .expect_err("untrusted target TLS");
    let error = policy.reqwest_error(&url, OutboundPhase::Tls, &raw);
    assert_eq!(error.route(), OutboundRouteKind::Direct);
    assert_eq!(error.phase(), OutboundPhase::Tls);
    assert_eq!(error.kind(), OutboundErrorKind::Transport);
    let projection = format!("{error:?} {error}");
    assert!(!projection.contains("target-tls-canary"));
    assert!(!projection.contains(&address.to_string()));
    server.finish();
}

/// Ensures CA bundles reject non-certificate PEM material atomically.
#[test]
fn custom_ca_bundle_rejects_private_keys_and_garbage() {
    let directory = tempfile::tempdir().expect("CA directory");
    let path = directory.path().join("provider-ca.pem");
    std::fs::write(
        &path,
        "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
    )
    .expect("write bundle");
    let policy = OutboundNetworkPolicy::from_environment(BTreeMap::new(), Some(path));
    assert!(policy.client_for("https://target.example").is_err());
}

/// Ensures an early-close selected proxy route never retries the same request
/// directly against an otherwise reachable target.
#[test]
fn selected_proxy_early_close_has_no_direct_fallback() {
    let target = DirectTargetCanary::new();
    let proxy = ScriptedTcpServer::spawn(|socket| {
        drop(socket);
    });
    let policy = policy(&[("http_proxy", &format!("http://{}", proxy.address()))]);
    let url = target.http_url("127.0.0.1");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let raw = runtime
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                policy.client_for(&url).expect("client").get(&url).send(),
            )
            .await
            .expect("proxy early-close request remained bounded")
        })
        .expect_err("selected proxy early close");
    let error = policy.reqwest_error(&url, OutboundPhase::Request, &raw);
    assert_eq!(error.route(), OutboundRouteKind::Proxy);
    assert_eq!(error.phase(), OutboundPhase::Request);
    assert_eq!(error.kind(), OutboundErrorKind::Transport);
    proxy.finish();
    target.assert_untouched();
}

/// Ensures selected-proxy DNS failure cannot trigger target DNS or a direct
/// request; the injected resolver makes both alternatives deterministic.
#[test]
fn selected_proxy_dns_failure_has_no_direct_fallback() {
    let target = DirectTargetCanary::new();
    let resolver = Arc::new(FailingProxyResolver::new(target.address()));
    let policy = policy(&[("http_proxy", "http://proxy.invalid:8080")]);
    let url = target.http_url("target.invalid");
    let client = policy
        .client_for_with_resolver(&url, Arc::clone(&resolver))
        .expect("route-fixed client");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let raw = runtime
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(2), client.get(&url).send())
                .await
                .expect("proxy DNS request remained bounded")
        })
        .expect_err("selected proxy DNS failure");
    let error = policy.reqwest_error(&url, OutboundPhase::Request, &raw);
    assert_eq!(error.route(), OutboundRouteKind::Proxy);
    assert_eq!(error.phase(), OutboundPhase::Proxy);
    assert_eq!(error.kind(), OutboundErrorKind::Transport);
    assert_eq!(resolver.queries(), ["proxy.invalid"]);
    target.assert_untouched();
}

/// Ensures an HTTPS proxy is authenticated with the same additive CA policy
/// before it receives a plain-HTTP target in absolute form.
#[test]
fn https_proxy_uses_custom_ca_and_absolute_form() {
    use std::io::{Read, Write};

    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().expect("CA key");
    let ca = ca_params.self_signed(&ca_key).expect("CA certificate");
    let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
    let leaf = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .expect("leaf params")
        .signed_by(&leaf_key, &ca, &ca_key)
        .expect("leaf certificate");
    let server = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                leaf_key.serialize_der(),
            )),
        )
        .expect("proxy TLS");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("proxy listener");
    let address = listener.local_addr().expect("proxy address");
    let proxy = std::thread::spawn(move || {
        let (socket, _) = listener.accept().expect("proxy connection");
        let connection = rustls::ServerConnection::new(Arc::new(server)).expect("proxy connection");
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("proxy request");
            request.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .expect("proxy response");
        String::from_utf8(request).expect("ASCII request")
    });
    let directory = tempfile::tempdir().expect("CA directory");
    let path = directory.path().join("proxy-ca.pem");
    std::fs::write(&path, ca.pem()).expect("write CA");
    let environment = BTreeMap::from([(
        "http_proxy".to_owned(),
        format!("https://localhost:{}", address.port()),
    )]);
    let policy = OutboundNetworkPolicy::from_environment(environment, Some(path));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let response = runtime
        .block_on(
            policy
                .client_for("http://unresolvable.invalid/secure-proxy")
                .expect("client")
                .get("http://unresolvable.invalid/secure-proxy")
                .send(),
        )
        .expect("HTTPS proxy response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let request = proxy.join().expect("proxy thread");
    assert!(
        request.starts_with("GET http://unresolvable.invalid/secure-proxy HTTP/1.1\r\n"),
        "request was {request:?}",
    );
}

/// Ensures untrusted HTTPS-proxy TLS fails before any proxy request and never
/// retries the otherwise reachable HTTP target directly.
#[test]
fn untrusted_https_proxy_tls_has_no_direct_fallback_and_is_redacted() {
    use std::io::Read;

    let target = DirectTargetCanary::new();
    let ca = TestCa::new();
    let proxy_tls = ca.server_config("localhost");
    let proxy = ScriptedTcpServer::spawn(move |socket| {
        let connection =
            rustls::ServerConnection::new(Arc::new(proxy_tls)).expect("proxy TLS state");
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let mut byte = [0_u8; 1];
        // This call is intentionally best-effort; preserve the existing discarded
        // result. ast-grep-ignore: let-underscore-call
        let _ = stream.read(&mut byte);
    });
    let address = proxy.address();
    let policy = policy(&[(
        "http_proxy",
        &format!("https://proxy-user:proxy-pass@localhost:{}", address.port()),
    )]);
    let url = target.http_url("127.0.0.1");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let raw = runtime
        .block_on(policy.client_for(&url).expect("client").get(&url).send())
        .expect_err("untrusted proxy TLS");
    let error = policy.reqwest_error(&url, OutboundPhase::Request, &raw);
    assert_eq!(error.route(), OutboundRouteKind::Proxy);
    assert_eq!(error.phase(), OutboundPhase::Proxy);
    assert_eq!(error.kind(), OutboundErrorKind::Transport);
    let projection = format!("{error:?} {error}");
    for canary in ["proxy-user", "proxy-pass", "localhost"] {
        assert!(
            !projection.contains(canary),
            "leaked {canary}: {projection}"
        );
    }
    proxy.finish();
    target.assert_untouched();
}

/// Ensures HTTPS proxying performs outer proxy TLS, an authenticated CONNECT,
/// inner target TLS, and an origin-form target request in that exact order.
#[test]
fn https_target_through_https_proxy_uses_nested_tls_and_scoped_basic_auth() {
    use std::io::Write;

    let proxy_ca = TestCa::new();
    let target_ca = TestCa::new();
    let proxy_tls = proxy_ca.server_config("localhost");
    let target_tls = target_ca.server_config("localhost");
    let proxy = ScriptedTcpServer::spawn(move |socket| {
        let outer = rustls::ServerConnection::new(Arc::new(proxy_tls)).expect("outer TLS");
        let mut outer = rustls::StreamOwned::new(outer, socket);
        let connect = read_http_head(&mut outer);
        outer
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .expect("CONNECT response");

        let inner = rustls::ServerConnection::new(Arc::new(target_tls)).expect("inner TLS");
        let mut inner = rustls::StreamOwned::new(inner, outer);
        let request = read_http_head(&mut inner);
        inner
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .expect("target response");
        (connect, request)
    });
    let address = proxy.address();
    let directory = tempfile::tempdir().expect("CA directory");
    let ca_path = directory.path().join("nested-ca.pem");
    std::fs::write(&ca_path, format!("{}\n{}", proxy_ca.pem(), target_ca.pem()))
        .expect("write CA bundle");
    let environment = BTreeMap::from([(
        "https_proxy".to_owned(),
        format!("https://proxy-user:proxy-pass@localhost:{}", address.port()),
    )]);
    let policy = OutboundNetworkPolicy::from_environment(environment, Some(ca_path));
    let url = "https://localhost:4443/nested";
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let response = runtime
        .block_on(
            policy
                .client_for(url)
                .expect("client")
                .get(url)
                .header("authorization", "Bearer target-canary")
                .send(),
        )
        .expect("nested TLS response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let (connect, request) = proxy.finish();
    assert!(connect.starts_with("CONNECT localhost:4443 HTTP/1.1\r\n"));
    assert_eq!(
        connect
            .lines()
            .filter(|line| line
                .to_ascii_lowercase()
                .starts_with("proxy-authorization:"))
            .collect::<Vec<_>>(),
        ["Proxy-Authorization: Basic cHJveHktdXNlcjpwcm94eS1wYXNz"]
    );
    assert!(!connect.contains("target-canary"));
    assert!(request.starts_with("GET /nested HTTP/1.1\r\n"));
    assert!(request.contains("authorization: Bearer target-canary\r\n"));
    assert!(
        !request
            .to_ascii_lowercase()
            .contains("proxy-authorization:")
    );
}

/// Ensures an ordinary HTTPS request through a cleartext HTTP proxy uses
/// CONNECT followed by verified target TLS and an origin-form request.
#[test]
fn https_target_through_http_proxy_uses_connect_and_target_tls() {
    use std::io::Write;

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
        let request = read_http_head(&mut tunnel);
        tunnel
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .expect("target response");
        (connect, request)
    });
    let directory = tempfile::tempdir().expect("CA directory");
    let ca_path = directory.path().join("target-ca.pem");
    std::fs::write(&ca_path, target_ca.pem()).expect("write target CA");
    let environment = BTreeMap::from([(
        "https_proxy".to_owned(),
        format!("http://{}", proxy.address()),
    )]);
    let policy = OutboundNetworkPolicy::from_environment(environment, Some(ca_path));
    let url = "https://localhost:4442/through-http-proxy";
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let response = runtime
        .block_on(policy.client_for(url).expect("client").get(url).send())
        .expect("HTTPS through HTTP proxy");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let (connect, request) = proxy.finish();
    assert!(
        connect.starts_with("CONNECT localhost:4442 HTTP/1.1\r\n"),
        "CONNECT was {connect:?}"
    );
    assert!(
        request.starts_with("GET /through-http-proxy HTTP/1.1\r\n"),
        "request was {request:?}"
    );
}

/// Ensures strict CA parsing accepts multiple certificate authorities and
/// deduplicates exact repeated DER without weakening certificate-only parsing.
#[test]
fn custom_ca_bundle_accepts_mixed_certificates_and_deduplicates_exact_der() {
    let first = TestCa::new();
    let second = TestCa::new();
    let directory = tempfile::tempdir().expect("CA directory");
    let path = directory.path().join("mixed.pem");
    std::fs::write(
        &path,
        format!("{}\n{}\n{}", first.pem(), second.pem(), first.pem()),
    )
    .expect("write mixed bundle");
    let roots = load_custom_roots(Some(path)).expect("strict mixed certificate bundle");
    assert_eq!(roots.len(), 2);
}

/// Ensures one malformed block rejects an otherwise valid CA bundle atomically.
#[test]
fn custom_ca_bundle_rejects_valid_plus_malformed_content_atomically() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("CA directory");
    let path = directory.path().join("malformed-mixed.pem");
    std::fs::write(
        &path,
        format!(
            "{}\n-----BEGIN CERTIFICATE-----\n%%%%\n-----END CERTIFICATE-----\n",
            ca.pem()
        ),
    )
    .expect("write malformed mixed bundle");
    let error = load_custom_roots(Some(path)).expect_err("mixed malformed bundle");
    assert_eq!(error.kind(), OutboundErrorKind::InvalidConfiguration);
}

/// Ensures each named provider-network environment variable handles non-UTF-8
/// values without panic or fallback: text settings reject decoding, while the
/// CA path remains an opaque path and fails closed when unreadable.
#[cfg(unix)]
#[test]
fn named_non_utf8_environment_values_fail_closed() {
    use std::os::unix::ffi::OsStringExt;

    const CHILD: &str = "TAU_TEST_NAMED_NON_UTF8_CHILD";
    if let Some(name) = std::env::var_os(CHILD) {
        let policy = OutboundNetworkPolicy::from_env();
        let error = policy
            .client_for("https://localhost")
            .expect_err("named non-UTF-8 value must poison snapshot");
        assert_eq!(error.phase(), OutboundPhase::Configure, "{name:?}");
        assert_eq!(error.kind(), OutboundErrorKind::InvalidConfiguration);
        return;
    }
    for name in [
        "http_proxy",
        "HTTP_PROXY",
        "https_proxy",
        "HTTPS_PROXY",
        "all_proxy",
        "ALL_PROXY",
        "no_proxy",
        "NO_PROXY",
        "TAU_PROVIDER_CA_BUNDLE",
    ] {
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("test executable"));
        command
            .arg("--exact")
            .arg("outbound_network::tests::named_non_utf8_environment_values_fail_closed")
            .arg("--nocapture")
            .env(CHILD, name)
            .env(name, std::ffi::OsString::from_vec(vec![0xff]));
        for key in [
            "http_proxy",
            "HTTP_PROXY",
            "https_proxy",
            "HTTPS_PROXY",
            "all_proxy",
            "ALL_PROXY",
            "no_proxy",
            "NO_PROXY",
            "TAU_PROVIDER_CA_BUNDLE",
        ] {
            if key != name {
                command.env_remove(key);
            }
        }
        assert!(command.status().expect("child test").success(), "{name}");
    }
}
