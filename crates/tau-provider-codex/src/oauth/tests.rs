use std::sync::mpsc;
use std::{io as path_std_io, net as path_std_net};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;

fn jwt(claims: serde_json::Value) -> String {
    use base64::Engine as _;
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("serialize claims"));
    format!("header.{payload}.signature")
}

/// Refresh parsing preserves every omitted replacement while accepting an
/// independently rotated refresh token.
#[test]
fn refresh_response_accepts_omitted_replacement_fields() {
    let omitted = super::parse_openai_refresh_response(&serde_json::json!({}))
        .expect("all replacements may be omitted");
    assert!(omitted.access_token.is_none());
    assert!(omitted.refresh_token.is_none());
    assert!(omitted.expires_at_ms.is_none());
    assert!(omitted.account_id.is_none());

    let rotated = super::parse_openai_refresh_response(&serde_json::json!({
        "refresh_token": "replacement"
    }))
    .expect("refresh-only rotation");
    assert_eq!(rotated.refresh_token.as_deref(), Some("replacement"));
    assert!(rotated.access_token.is_none());
}

/// A replacement access token must be a non-expired JWT with a bounded numeric
/// expiry; provider `expires_in` cannot override that authority.
#[test]
fn refresh_response_rejects_malformed_and_expired_access_tokens() {
    for access_token in [
        "malformed".to_owned(),
        jwt(serde_json::json!({"exp": "later"})),
        jwt(serde_json::json!({"exp": 1})),
    ] {
        assert!(
            super::parse_openai_refresh_response(&serde_json::json!({
                "access_token": access_token,
                "expires_in": u64::MAX,
            }))
            .is_err()
        );
    }
}

/// A successful refresh envelope must remain an object even when every
/// replacement field is omitted.
#[test]
fn refresh_response_rejects_non_object_json() {
    for value in [
        serde_json::Value::Null,
        serde_json::json!([]),
        serde_json::json!("text"),
        serde_json::json!(1),
    ] {
        assert!(super::parse_openai_refresh_response(&value).is_err());
    }
}

/// Oversized numeric `iat` claims cannot panic later refresh scheduling.
#[test]
fn jwt_issued_at_rejects_millisecond_overflow() {
    let token = jwt(serde_json::json!({
        "iat": u64::MAX,
        "exp": u64::MAX / 1000,
    }));
    assert!(super::jwt_issued_at_ms(&token).is_none());
    assert!(super::jwt_expiration_ms(&token).is_some());
}

/// Refresh uses the current JSON contract and never regresses auth-code
/// exchange's separate form-encoded request.
#[test]
fn refresh_request_fields_serialize_as_json() {
    let body = super::refresh_request("opaque +& token");
    let encoded = body.to_string();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&encoded).expect("JSON request"),
        body
    );
    assert!(!encoded.contains("grant_type=refresh_token"));
}

/// The actual HTTP helpers keep refresh JSON separate from the form-encoded
/// authorization-code exchange path.
#[test]
fn oauth_http_paths_send_distinct_wire_encodings() {
    let refresh = capture_oauth_request(|url| {
        super::post_json(
            url,
            &super::refresh_request("opaque +& token"),
            &crate::test_network_policy(),
        )
    });
    assert!(refresh.contains("content-type: application/json\r\n"));
    let refresh_body = refresh.split_once("\r\n\r\n").expect("request body").1;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(refresh_body).expect("refresh JSON"),
        super::refresh_request("opaque +& token")
    );

    let exchange = capture_oauth_request(|url| {
        super::post_form(
            url,
            "grant_type=authorization_code&code=opaque%2B%26",
            &crate::test_network_policy(),
        )
    });
    assert!(exchange.contains("content-type: application/x-www-form-urlencoded\r\n"));
    assert!(exchange.ends_with("grant_type=authorization_code&code=opaque%2B%26"));
}

fn capture_oauth_request(
    request: impl FnOnce(&str) -> Result<serde_json::Value, super::OAuthError>,
) -> String {
    let listener =
        path_std_net::TcpListener::bind(("127.0.0.1", 0)).expect("bind OAuth test server");
    let address = listener.local_addr().expect("OAuth test server address");
    let (sender, receiver) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept OAuth request");
        let bytes = read_complete_http_request_bytes(&mut stream);
        sender.send(bytes).expect("capture OAuth request");
        let body = b"{}";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        path_std_io::Write::write_all(&mut stream, response.as_bytes()).expect("write headers");
        path_std_io::Write::write_all(&mut stream, body).expect("write body");
    });
    request(&format!("http://{address}/oauth/token")).expect("OAuth request");
    server.join().expect("OAuth test server");
    String::from_utf8(receiver.recv().expect("captured request")).expect("ASCII request")
}

/// OpenAI's nested OAuth envelope must expose only typed, bounded fields rather
/// than retaining the complete response body.
#[test]
fn nested_openai_error_envelope_is_typed_and_bounded() {
    let error = super::OAuthError::http(
        401,
        Some(
            r#"{"error":{"message":"refresh token was\nalready used","type":"invalid_request_error","code":"refresh_token_reused"},"secret":"must-not-escape"}"#,
        ),
    );

    assert_eq!(error.kind(), super::OAuthErrorKind::Http);
    assert_eq!(error.http_status(), Some(401));
    assert_eq!(error.provider_code(), Some("refresh_token_reused"));
    assert_eq!(error.message(), Some("refresh token was already used"));
    let rendered = error.to_string();
    assert_eq!(
        rendered,
        "OAuth request was rejected (HTTP 401) [refresh_token_reused]"
    );
    assert!(!rendered.contains("must-not-escape"));
    assert!(!rendered.contains('\n'));
}

/// Provider codes and messages are bounded before storage so repeated logging
/// cannot amplify an arbitrarily large OAuth response.
#[test]
fn oauth_error_fields_and_repeated_rendering_remain_bounded() {
    let code = format!("prefix_{}secret-code-tail", "c".repeat(1_000));
    let message = format!("first line\n{}\nsecret-message-tail", "m".repeat(10_000));
    let body = serde_json::json!({
        "error": {
            "code": code,
            "message": message,
        }
    })
    .to_string();
    let error = super::OAuthError::http(400, Some(&body));

    assert!(
        error
            .provider_code()
            .expect("bounded provider code")
            .ends_with('…')
    );
    assert!(
        error
            .message()
            .expect("bounded provider message")
            .ends_with('…')
    );
    for _ in 0..32 {
        let rendered = error.to_string();
        assert!(rendered.chars().count() < 400);
        assert!(!rendered.contains("secret-code-tail"));
        assert!(!rendered.contains("secret-message-tail"));
        assert!(!rendered.contains('\n'));
    }
}

/// Malformed and non-JSON error bodies must degrade to status-only failures and
/// must never be copied into diagnostics.
#[test]
fn malformed_oauth_error_body_is_not_rendered() {
    let raw = "<html>\nupstream secret body\n</html>";
    let error = super::OAuthError::http(502, Some(raw));

    assert_eq!(error.http_status(), Some(502));
    assert_eq!(error.provider_code(), None);
    assert_eq!(error.message(), None);
    assert_eq!(error.to_string(), "OAuth request was rejected (HTTP 502)");
    assert!(!error.to_string().contains(raw));
}

/// The HTTP reader must stop oversized OAuth error bodies before parsing and
/// return the same status-only error with credential-safe default formatting.
#[test]
fn oversized_oauth_error_body_is_not_retained() {
    let listener =
        path_std_net::TcpListener::bind(("127.0.0.1", 0)).expect("bind OAuth test server");
    let address = listener.local_addr().expect("OAuth test server address");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept OAuth request");
        read_complete_http_request(&mut stream);
        let body = format!(
            r#"{{"error":{{"code":"oversized","message":"{}secret-tail"}}}}"#,
            "x".repeat(super::MAX_OAUTH_RESPONSE_BODY_BYTES as usize)
        );
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        path_std_io::Write::write_all(&mut stream, response.as_bytes())
            .expect("write OAuth response");
    });

    let error = super::post_form(
        &format!("http://{address}/oauth/token"),
        "grant_type=test",
        &crate::test_network_policy(),
    )
    .expect_err("oversized response must fail");
    server.join().expect("OAuth test server");

    assert_eq!(error.http_status(), Some(400));
    assert_eq!(error.provider_code(), None);
    assert_eq!(error.message(), None);
    assert!(!error.to_string().contains("secret-tail"));
}

/// Standard flat OAuth envelopes remain supported alongside OpenAI's nested
/// variant, without putting provider prose in the safe log projection.
#[test]
fn flat_oauth_error_envelope_is_parsed() {
    let error = super::OAuthError::from_http_response(
        400,
        r#"{"error":"invalid_grant","error_description":"sentinel provider message"}"#,
    );

    assert_eq!(error.provider_code(), Some("invalid_grant"));
    assert_eq!(error.message(), Some("sentinel provider message"));
    assert_eq!(
        error.to_string(),
        "OAuth request was rejected (HTTP 400) [invalid_grant]"
    );
    assert!(!format!("{error:?}").contains("sentinel"));
}

/// Null or non-string preferred fields must not hide valid lower-precedence
/// nested code and description fields.
#[test]
fn nested_error_null_fields_fall_back_to_typed_strings() {
    let error = super::OAuthError::from_http_response(
        400,
        r#"{"error":{"code":null,"type":"invalid_grant","message":null,"error_description":"rejected"}}"#,
    );

    assert_eq!(error.provider_code(), Some("invalid_grant"));
    assert_eq!(error.message(), Some("rejected"));
}

/// Display and Debug are the credential-safe projections used by traces; even
/// recognized envelope fields cannot reflect arbitrary credential sentinels.
#[test]
fn oauth_error_formatting_excludes_arbitrary_provider_fields() {
    let secret = "oauth-secret-sentinel-123";
    let body = serde_json::json!({
        "error": {
            "code": secret,
            "message": format!("provider reflected {secret}"),
        }
    })
    .to_string();
    let error = super::OAuthError::from_http_response(401, &body);

    assert!(error.provider_code().is_some());
    assert!(error.message().is_some());
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{error:?}").contains(secret));
}

/// The public envelope constructor enforces the same byte cap as HTTP reads
/// before attempting JSON parsing.
#[test]
fn public_oauth_error_constructor_rejects_oversized_body_before_parsing() {
    let secret = "oversized-constructor-secret";
    let body = format!(
        r#"{{"error":{{"code":"invalid_grant","message":"{}-{secret}"}}}}"#,
        "x".repeat(super::MAX_OAUTH_RESPONSE_BODY_BYTES as usize)
    );
    let error = super::OAuthError::from_http_response(400, &body);

    assert_eq!(error.kind(), super::OAuthErrorKind::Http);
    assert_eq!(error.provider_code(), None);
    assert_eq!(error.message(), None);
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{error:?}").contains(secret));
}

/// The response cap accepts a valid document of exactly the advertised size;
/// the reader's internal EOF probe must not make the public bound off by one.
#[test]
fn exact_size_oauth_success_body_is_accepted() {
    let prefix = r#"{"access_token":"a","refresh_token":"r","expires_in":1,"padding":""#;
    let suffix = r#""}"#;
    let padding = super::MAX_OAUTH_RESPONSE_BODY_BYTES as usize - prefix.len() - suffix.len();
    let body = format!("{prefix}{}{suffix}", "x".repeat(padding));
    assert_eq!(body.len(), super::MAX_OAUTH_RESPONSE_BODY_BYTES as usize);

    let result = post_form_from_test_server("200 OK", body).expect("exact-cap OAuth response");
    assert_eq!(result["access_token"], "a");
}

/// A successful HTTP response above the body cap is a typed invalid response,
/// not a retry-looking transport failure.
#[test]
fn oversized_oauth_success_body_is_invalid_response() {
    let body = "x".repeat(super::MAX_OAUTH_RESPONSE_BODY_BYTES as usize + 1);
    let error =
        post_form_from_test_server("200 OK", body).expect_err("oversized OAuth response must fail");

    assert_eq!(error.kind(), super::OAuthErrorKind::InvalidResponse);
    assert!(!error.to_string().contains('x'));
}

/// A complete bounded 2xx body with invalid UTF-8 is provider-invalid data,
/// not a transport failure, and its bytes never enter formatting.
#[test]
fn invalid_utf8_oauth_success_body_is_invalid_response() {
    let error = post_form_bytes_from_test_server("200 OK", vec![0xff, 0xfe])
        .expect_err("invalid UTF-8 OAuth response must fail");

    assert_eq!(error.kind(), super::OAuthErrorKind::InvalidResponse);
    assert_eq!(error.to_string(), "OAuth response was invalid");
    assert!(!format!("{error:?}").contains("255"));
}

fn post_form_from_test_server(
    status: &'static str,
    body: String,
) -> Result<serde_json::Value, super::OAuthError> {
    post_form_bytes_from_test_server(status, body.into_bytes())
}

fn post_form_bytes_from_test_server(
    status: &'static str,
    body: Vec<u8>,
) -> Result<serde_json::Value, super::OAuthError> {
    let listener =
        path_std_net::TcpListener::bind(("127.0.0.1", 0)).expect("bind OAuth test server");
    let address = listener.local_addr().expect("OAuth test server address");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept OAuth request");
        read_complete_http_request(&mut stream);
        let headers = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = path_std_io::Write::write_all(&mut stream, headers.as_bytes());
        let _ = path_std_io::Write::write_all(&mut stream, &body);
    });
    let result = super::post_form(
        &format!("http://{address}/oauth/token"),
        "grant_type=test",
        &crate::test_network_policy(),
    );
    server.join().expect("OAuth test server");
    result
}

/// Drain the complete bounded request before closing a test-server socket.
///
/// Closing a TCP socket with unread request bytes can send a reset instead of a
/// clean response EOF. Under parallel test load that made response-validation
/// tests intermittently observe a transport error rather than the intended
/// oversized or invalid-encoding classification.
fn read_complete_http_request(stream: &mut std::net::TcpStream) {
    let _ = read_complete_http_request_bytes(stream);
}

fn read_complete_http_request_bytes(stream: &mut std::net::TcpStream) -> Vec<u8> {
    const MAX_REQUEST_BYTES: usize = 8 * 1024;

    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = path_std_io::Read::read(stream, &mut buffer).expect("read OAuth request");
        assert!(read > 0, "OAuth request ended before its headers");
        request.extend_from_slice(&buffer[..read]);
        assert!(
            request.len() <= MAX_REQUEST_BYTES,
            "OAuth test request exceeded {MAX_REQUEST_BYTES} bytes"
        );

        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let body_start = header_end + 4;
        let headers = std::str::from_utf8(&request[..header_end]).expect("ASCII OAuth headers");
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| {
                    value
                        .trim()
                        .parse::<usize>()
                        .expect("numeric Content-Length")
                })
            })
            .unwrap_or(0);
        if request.len() >= body_start + content_length {
            return request;
        }
    }
}

/// Only explicit credential-invalidating provider codes may suppress future
/// refreshes; status alone is not enough to classify an ambiguous outage.
#[test]
fn permanent_refresh_rejection_requires_known_provider_code() {
    for code in [
        "invalid_grant",
        "invalid_refresh_token",
        "refresh_token_reused",
        "refresh_token_revoked",
    ] {
        let body = serde_json::json!({"error": {"code": code}}).to_string();
        assert!(super::OAuthError::from_http_response(400, &body).is_permanent_refresh_rejection());
        assert!(super::OAuthError::from_http_response(401, &body).is_permanent_refresh_rejection());
    }

    assert!(
        !super::OAuthError::from_http_response(
            401,
            r#"{"error":{"code":"temporarily_unavailable"}}"#,
        )
        .is_permanent_refresh_rejection()
    );
    assert!(
        !super::OAuthError::from_http_response(401, "malformed").is_permanent_refresh_rejection()
    );
    assert!(
        !super::OAuthError::from_http_response(
            500,
            r#"{"error":{"code":"refresh_token_reused"}}"#,
        )
        .is_permanent_refresh_rejection()
    );
}
