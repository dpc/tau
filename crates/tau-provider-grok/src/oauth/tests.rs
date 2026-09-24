//! Synthetic OAuth wire tests; these do not log in or contact xAI.

use std::future::{pending, ready};
use std::net::TcpListener as StdTcpListener;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime;
use tokio::sync::oneshot;

use super::*;

/// Local fake issuer and recorded form requests, with bounded accept/read
/// waits.
struct Server {
    /// Test-only loopback issuer URL.
    issuer: String,
    /// Complete requests accepted by the fake server.
    requests: Arc<Mutex<Vec<String>>>,
    /// Thread owns only non-secret synthetic test values.
    worker: std::thread::JoinHandle<()>,
}

impl Server {
    /// Serve exactly the supplied replies, recording one request per
    /// connection.
    fn start(replies: Vec<(u16, String)>) -> Self {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind fake issuer");
        let issuer = format!("http://{}", listener.local_addr().expect("local address"));
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let worker = std::thread::spawn(move || {
            runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("server runtime")
                .block_on(async {
                    let listener = TcpListener::from_std(listener).expect("async listener");
                    for (status, body) in replies {
                        let (mut stream, _) = tokio::time::timeout(
                            Duration::from_secs(10),
                            listener.accept(),
                        )
                        .await
                        .expect("request timeout")
                        .expect("accept");
                        let request = read_request(&mut stream).await;
                        recorded.lock().expect("request lock").push(request);
                        let response = format!(
                            "HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream.write_all(response.as_bytes()).await.expect("reply");
                    }
                });
        });
        Self {
            issuer,
            requests,
            worker,
        }
    }

    /// Construct a test-only client; production cannot substitute OAuth
    /// origins.
    fn client(&self) -> Client {
        client_at(&self.issuer)
    }

    /// Wait for every expected request and return its recorded bytes.
    fn finish(self) -> Vec<String> {
        self.worker.join().expect("fake issuer worker");
        Arc::try_unwrap(self.requests)
            .expect("single recording owner")
            .into_inner()
            .expect("request recording")
    }
}

/// Build through the production no-retry path without ambient proxy settings.
fn client_at(issuer: &str) -> Client {
    Client::at_issuer(
        "tau-test-client",
        &tau_provider::OutboundNetworkPolicy::from_environment(Default::default(), None),
        issuer,
    )
    .expect("HTTP client")
}

/// Read a complete synthetic HTTP request with a real deadline, not a ticker.
async fn read_request(stream: &mut TcpStream) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut request = Vec::new();
        let mut buffer = [0; 4_096];
        loop {
            let count = stream.read(&mut buffer).await.expect("read request");
            assert_ne!(count, 0, "request ended before body");
            request.extend_from_slice(&buffer[..count]);
            let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(|value| value.parse::<usize>().expect("content length"))
                })
                .unwrap_or(0);
            if header_end + 4 + length <= request.len() {
                return String::from_utf8(request).expect("ASCII fixture");
            }
        }
    })
    .await
    .expect("request read timeout")
}

/// Construct a short-lived synthetic device grant without waiting real seconds.
fn device() -> DeviceAuthorization {
    DeviceAuthorization {
        device_code: "device+&=code".to_owned(),
        user_code: "ABCD-EFGH".to_owned(),
        verification_uri: "https://auth.x.ai/device".to_owned(),
        deadline: Instant::now() + Duration::from_secs(10),
        interval: Duration::from_millis(1),
    }
}

/// Parse unknown lifetimes and rotation omission without inventing
/// replacements.
#[test]
fn token_response_preserves_unknown_expiry_and_rotation() {
    let tokens = TokenResponse::parse(br#"{"access_token":"opaque","token_type":"Bearer"}"#)
        .expect("valid opaque access token");
    assert_eq!(tokens.access_token, "opaque");
    assert!(tokens.refresh_token.is_none());
    assert!(tokens.expires_in.is_none());
    let tokens = TokenResponse::parse(
        br#"{"access_token":"new","refresh_token":"rotated","expires_in":90}"#,
    )
    .expect("rotated tokens");
    assert_eq!(tokens.refresh_token.as_deref(), Some("rotated"));
    assert_eq!(tokens.expires_in, Some(90));
}

/// Reject malformed successful token responses before callers can save them.
#[test]
fn token_response_rejects_bad_fields() {
    for body in [
        "{}",
        "[]",
        r#"{"access_token":""}"#,
        r#"{"access_token":"a\nb"}"#,
        r#"{"access_token":"a","refresh_token":""}"#,
        r#"{"access_token":"a","token_type":"Basic"}"#,
        r#"{"access_token":"a","expires_in":0}"#,
        r#"{"access_token":"a","expires_in":-1}"#,
        r#"{"access_token":"a","expires_in":"60"}"#,
    ] {
        assert!(matches!(
            TokenResponse::parse(body.as_bytes()),
            Err(Error::InvalidResponse)
        ));
    }
}

/// Bound device lifetime and reject misleading browser destinations/control
/// text.
#[test]
fn device_response_validation_and_schedule() {
    let started = Instant::now();
    let valid = serde_json::json!({
        "device_code": "opaque", "user_code": "ABCD-EFGH",
        "verification_uri": "https://auth.x.ai/device", "expires_in": 600
    });
    let parse = |value: &serde_json::Value| {
        DeviceAuthorization::parse(&serde_json::to_vec(value).expect("fixture"), started)
    };
    let mut grant = parse(&valid).expect("valid grant");
    assert_eq!(grant.interval, Duration::from_secs(5));
    grant.slow_down();
    grant.slow_down();
    assert_eq!(grant.interval, Duration::from_secs(15));
    assert_eq!(grant.deadline, started + Duration::from_secs(600));
    for (key, bad) in [
        ("device_code", serde_json::json!("")),
        ("user_code", serde_json::json!("code\n")),
        (
            "verification_uri",
            serde_json::json!("http://auth.x.ai/device"),
        ),
        (
            "verification_uri",
            serde_json::json!("https://user@auth.x.ai"),
        ),
        ("expires_in", serde_json::json!(0)),
        ("interval", serde_json::json!(0)),
    ] {
        let mut value = valid.clone();
        value[key] = bad;
        assert!(matches!(parse(&value), Err(Error::InvalidResponse)));
    }
    let mut huge = valid;
    huge["expires_in"] = serde_json::json!(u64::MAX);
    huge["interval"] = serde_json::json!(u64::MAX);
    let bounded = parse(&huge).expect("bounded lifetime");
    assert_eq!(bounded.deadline, started + Duration::from_secs(1_800));
    assert_eq!(bounded.interval, Duration::from_secs(1_800));
}

/// Device flow uses only xAI form fields and exposes no fake Build identity.
#[tokio::test]
async fn device_flow_pending_then_success() {
    let server = Server::start(vec![
        (400, r#"{"error":"authorization_pending"}"#.to_owned()),
        (
            200,
            r#"{"access_token":"access","refresh_token":"refresh","expires_in":60}"#.to_owned(),
        ),
    ]);
    let client = server.client();
    let tokens = client
        .finish_device(device(), pending())
        .await
        .expect("approved device");
    assert_eq!(tokens.access_token, "access");
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert!(request.starts_with("POST /oauth2/token "));
        assert!(request.contains("device_code=device%2B%26%3Dcode"));
        assert!(request.contains("client_id=tau-test-client"));
        assert!(request.contains("user-agent: tau/"));
        assert!(!request.contains("grok-client"));
        assert!(!request.contains("grok-shell"));
    }
}

/// A complete grant rejection exposes only a closed, credential-free error.
#[tokio::test]
async fn refresh_rejection_and_safe_formatting() {
    let server = Server::start(vec![(
        400,
        r#"{"error":"invalid_grant","error_description":"secret-refresh-value"}"#.to_owned(),
    )]);
    let error = match server.client().refresh("r+&=").await {
        Ok(_) => panic!("rejected refresh succeeded"),
        Err(error) => error,
    };
    assert_eq!(error, Error::InvalidGrant);
    assert!(error.rejects_refresh_generation());
    assert!(!format!("{error} {error:?}").contains("secret-refresh-value"));
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("refresh_token=r%2B%26%3D"));
}

/// Missing replacement refresh and expiry fields remain absent after HTTP
/// parsing.
#[tokio::test]
async fn refresh_omission_and_server_validated_subject() {
    let server = Server::start(vec![
        (200, r#"{"access_token":"new-access"}"#.to_owned()),
        (200, r#"{"sub":"account-123","email":"ignored"}"#.to_owned()),
    ]);
    let client = server.client();
    let tokens = client.refresh("old-refresh").await.expect("refresh");
    assert!(tokens.refresh_token.is_none());
    assert!(tokens.expires_in.is_none());
    assert_eq!(
        client.subject(&tokens.access_token).await.expect("subject"),
        "account-123"
    );
    let requests = server.finish();
    assert!(requests[1].starts_with("GET /oauth2/userinfo "));
    assert!(requests[1].contains("authorization: Bearer new-access"));
}

/// Unknown provider errors and oversized bodies never expose untrusted details.
#[tokio::test]
async fn response_bounds_and_unknown_errors() {
    let server = Server::start(vec![
        (200, "x".repeat(MAX_RESPONSE_BYTES + 1)),
        (
            500,
            r#"{"error":"token-in-error","error_description":"secret"}"#.to_owned(),
        ),
    ]);
    let client = server.client();
    assert!(matches!(
        client.refresh("refresh").await,
        Err(Error::InvalidResponse)
    ));
    assert!(matches!(
        client.refresh("refresh").await,
        Err(Error::Rejected)
    ));
    assert_eq!(server.finish().len(), 2);
}

/// Cancellation and expiry stop before sending any device-code credential.
#[tokio::test]
async fn canceled_and_expired_device_do_not_dispatch() {
    let server = Server::start(Vec::new());
    let client = server.client();
    assert!(matches!(
        client.finish_device(device(), ready(())).await,
        Err(Error::Canceled)
    ));
    let mut expired = device();
    expired.deadline = Instant::now();
    assert!(matches!(
        client.finish_device(expired, pending()).await,
        Err(Error::Expired)
    ));
    assert!(server.finish().is_empty());
}

/// Device denial and a success without a refresh credential are terminal
/// failures.
#[tokio::test]
async fn denied_and_nonrenewable_devices_fail() {
    let server = Server::start(vec![
        (400, r#"{"error":"access_denied"}"#.to_owned()),
        (200, r#"{"access_token":"short-lived"}"#.to_owned()),
    ]);
    let client = server.client();
    assert!(matches!(
        client.finish_device(device(), pending()).await,
        Err(Error::AccessDenied)
    ));
    assert!(matches!(
        client.finish_device(device(), pending()).await,
        Err(Error::InvalidResponse)
    ));
    assert_eq!(server.finish().len(), 2);
}

/// Device authorization requests only inference scopes, not workspace or
/// billing.
#[tokio::test]
async fn authorize_device_requests_inference_scopes() {
    let server = Server::start(vec![(
        200,
        r#"{"device_code":"private","user_code":"ABCD","verification_uri":"https://auth.x.ai/device","expires_in":600}"#.to_owned(),
    )]);
    let device = server
        .client()
        .authorize_device()
        .await
        .expect("device code");
    assert_eq!(device.user_code, "ABCD");
    let requests = server.finish();
    assert!(requests[0].starts_with("POST /oauth2/device/code "));
    assert!(
        requests[0]
            .contains("scope=openid+profile+email+offline_access+grok-cli%3Aaccess+api%3Aaccess")
    );
    assert!(!requests[0].contains("billing"));
    assert!(!requests[0].contains("workspace"));
}

/// Slow-down changes the real loop schedule instead of hammering the token URL.
#[tokio::test(start_paused = true)]
async fn slow_down_expires_before_an_early_second_poll() {
    let started = Instant::now();
    let mut grant = device();
    grant.interval = Duration::from_secs(5);
    grant.deadline = started + Duration::from_secs(10);
    let mut dispatches = Vec::new();
    assert!(matches!(
        poll_device(grant, pending(), || {
            dispatches.push(Instant::now());
            ready(Err(Error::SlowDown))
        })
        .await,
        Err(Error::Expired)
    ));
    assert_eq!(dispatches, [started + Duration::from_secs(5)]);
    assert_eq!(Instant::now(), started + Duration::from_secs(10));
}

/// Every slow-down permanently adds five seconds to later polling intervals.
#[tokio::test(start_paused = true)]
async fn repeated_slow_down_preserves_increased_interval() {
    let started = Instant::now();
    let mut grant = device();
    grant.interval = Duration::from_secs(5);
    grant.deadline = started + Duration::from_secs(60);
    let mut dispatches = Vec::new();
    let tokens = poll_device(grant, pending(), || {
        dispatches.push(Instant::now());
        ready(if dispatches.len() < 3 {
            Err(Error::SlowDown)
        } else {
            Ok(br#"{"access_token":"access","refresh_token":"refresh"}"#.to_vec())
        })
    })
    .await
    .expect("approved");
    assert_eq!(tokens.access_token, "access");
    assert_eq!(
        dispatches,
        [5, 15, 30].map(|seconds| started + Duration::from_secs(seconds))
    );
}

/// Cancellation drops an outstanding exchange rather than waiting its timeout.
#[tokio::test(start_paused = true)]
async fn in_flight_device_cancellation_is_prompt() {
    let started = Instant::now();
    let mut dispatched = false;
    let result = poll_device(
        device(),
        tokio::time::sleep_until(started + Duration::from_secs(1)),
        || {
            dispatched = true;
            pending()
        },
    )
    .await;
    assert!(matches!(result, Err(Error::Canceled)));
    assert!(dispatched);
    assert_eq!(Instant::now(), started + Duration::from_secs(1));
}

/// Expiry drops an outstanding exchange even if its HTTP future never resolves.
#[tokio::test(start_paused = true)]
async fn in_flight_device_expiry_is_absolute() {
    let started = Instant::now();
    let mut grant = device();
    grant.deadline = started + Duration::from_secs(2);
    let mut dispatched = false;
    let result = poll_device(grant, pending(), || {
        dispatched = true;
        pending()
    })
    .await;
    assert!(matches!(result, Err(Error::Expired)));
    assert!(dispatched);
    assert_eq!(Instant::now(), started + Duration::from_secs(2));
}

/// A consumed refresh POST with a lost response is not resent transparently.
#[tokio::test]
async fn lost_refresh_response_is_not_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let issuer = format!("http://{}", listener.local_addr().expect("address"));
    let client = client_at(&issuer);
    let (done, mut completion) = oneshot::channel::<()>();
    let worker = tokio::spawn(async move {
        let mut requests = Vec::new();
        loop {
            tokio::select! {
                // Drain an already-queued retry before observing completion.
                biased;
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.expect("accept");
                    requests.push(read_request(&mut stream).await);
                    // The token may be consumed; provide no HTTP response.
                    drop(stream);
                }
                _ = &mut completion => return requests,
            }
        }
    });
    let result = tokio::time::timeout(Duration::from_secs(5), client.refresh("single-use"))
        .await
        .expect("finite refresh");
    done.send(()).expect("stop counting peer");
    let requests = worker.await.expect("counting peer");
    assert!(matches!(result, Err(Error::Transport)));
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("refresh_token=single-use"));
}
