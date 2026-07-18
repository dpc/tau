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
    let mut environment = BTreeMap::from([(
        "http_proxy".to_owned(),
        "http://proxy.example:8080".to_owned(),
    )]);
    let policy = OutboundNetworkPolicy::from_environment(environment.clone(), None);
    environment.clear();
    assert_eq!(
        policy
            .route_kind("http://target.example")
            .expect("captured route"),
        OutboundRouteKind::Proxy
    );
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

/// Ensures a failed selected proxy route never retries the same request
/// directly against an otherwise reachable target.
#[test]
fn selected_proxy_failure_has_no_direct_fallback() {
    let target = std::net::TcpListener::bind("127.0.0.1:0").expect("target listener");
    target.set_nonblocking(true).expect("nonblocking target");
    let target_address = target.local_addr().expect("target address");
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").expect("proxy listener");
    let proxy_address = proxy.local_addr().expect("proxy address");
    let proxy = std::thread::spawn(move || {
        let (socket, _) = proxy.accept().expect("selected proxy connection");
        drop(socket);
    });
    let policy = policy(&[("http_proxy", &format!("http://{proxy_address}"))]);
    let url = format!("http://{target_address}/must-not-connect");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    assert!(
        runtime
            .block_on(policy.client_for(&url).expect("client").get(&url).send())
            .is_err()
    );
    proxy.join().expect("proxy thread");
    assert!(
        target.accept().is_err(),
        "failed proxy route silently reached direct target"
    );
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
