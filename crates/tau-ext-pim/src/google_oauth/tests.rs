use std::any::TypeId;

use super::*;
use crate::calendar::CalendarAccountId;
use crate::email::EmailAccountId;

/// Every OAuth semantic value must redact its contents even when the value is
/// otherwise authorized for explicit user-code display.
#[test]
fn debug_redacts_every_oauth_semantic_value() {
    assert_eq!(
        format!(
            "{:?}",
            AccessToken::from_validated_provider("access-secret".to_owned())
        ),
        "AccessToken(<redacted>)"
    );
    assert_eq!(
        format!(
            "{:?}",
            RefreshToken::from_validated_provider("refresh-secret".to_owned())
        ),
        "RefreshToken(<redacted>)"
    );
    assert_eq!(
        format!(
            "{:?}",
            DeviceCode::from_validated_provider("device-secret".to_owned())
        ),
        "DeviceCode(<redacted>)"
    );
    assert_eq!(
        format!(
            "{:?}",
            UserCode::from_validated_provider("user-secret".to_owned())
        ),
        "UserCode(<redacted>)"
    );
    assert_eq!(
        format!(
            "{:?}",
            AuthorizationCode::from_validated_redirect("authorization-secret".to_owned())
        ),
        "AuthorizationCode(<redacted>)"
    );
    assert_eq!(
        format!(
            "{:?}",
            OauthState::from_generator("state-secret".to_owned())
        ),
        "OauthState(<redacted>)"
    );
    assert_eq!(
        format!(
            "{:?}",
            PkceVerifier::from_generator("verifier-secret".to_owned())
        ),
        "PkceVerifier(<redacted>)"
    );
    assert_eq!(
        format!(
            "{:?}",
            LoopbackRedirectUri::from_generator("redirect-secret".to_owned())
        ),
        "LoopbackRedirectUri(<redacted>)"
    );
}

/// Each OAuth meaning must remain a distinct nominal class so an adapter
/// cannot accidentally substitute one same-spelled string for another.
#[test]
fn oauth_semantic_values_are_non_interchangeable_types() {
    let types = [
        TypeId::of::<AccessToken>(),
        TypeId::of::<RefreshToken>(),
        TypeId::of::<DeviceCode>(),
        TypeId::of::<UserCode>(),
        TypeId::of::<AuthorizationCode>(),
        TypeId::of::<OauthState>(),
        TypeId::of::<PkceVerifier>(),
        TypeId::of::<LoopbackRedirectUri>(),
    ];
    for (index, current) in types.iter().enumerate() {
        assert!(
            types[index + 1..].iter().all(|other| current != other),
            "OAuth semantic types must remain distinct"
        );
    }
}

/// Ensures Google OAuth JSON parsing accepts Google's documented
/// verification_uri spelling and keeps token values out of errors.
#[test]
fn parses_device_authorization_response() {
    let start = parse_device_auth_start(
        r#"{"device_code":"device","user_code":"ABCD-EFGH","verification_uri":"https://example.test","expires_in":900}"#,
    )
    .expect("device authorization response parses");
    assert_eq!(start.device_code.expose_for_provider(), "device");
    assert_eq!(start.interval_secs, 5);
}

/// Ensures malformed token fields produce generic field names instead of
/// echoing the unsafe token value into diagnostics.
#[test]
fn rejects_unsafe_oauth_field_without_echoing_value() {
    let err = parse_access_token_response(
        "{\"access_token\":\"secret\\u0001value\",\"expires_in\":3600}",
        "Google token response",
    )
    .expect_err("unsafe access token is rejected");
    assert_eq!(err, "Google OAuth field `access_token` was invalid");
    assert!(!err.contains("secret"));
}

/// Ensures PKCE challenge generation matches the RFC 7636 appendix B
/// example so Gmail installed-app auth remains provider-compatible.
#[test]
fn pkce_s256_challenge_matches_rfc7636_example() {
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    assert!(is_valid_pkce_verifier(verifier));
    assert!(is_valid_pkce_verifier(&"A".repeat(43)));
    assert!(is_valid_pkce_verifier(&"A".repeat(128)));
    assert!(!is_valid_pkce_verifier(&"A".repeat(42)));
    assert!(!is_valid_pkce_verifier(&"A".repeat(129)));
    assert!(!is_valid_pkce_verifier(&"!".repeat(43)));
    assert_eq!(
        pkce_s256_challenge(verifier),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

/// Ensures the Gmail authorization URL contains the installed-app PKCE
/// parameters Google requires, without exposing the verifier.
#[test]
fn installed_app_authorization_url_contains_pkce_and_offline_access() {
    let url = build_installed_app_authorization_url(
        "client-id",
        "http://127.0.0.1:54321/",
        GOOGLE_MAIL_SCOPE,
        "state-secret",
        "challenge-secret",
    )
    .expect("authorization URL builds");
    let parsed = Url::parse(&url).expect("URL parses");
    let query = parsed.query_pairs().collect::<BTreeMap<_, _>>();
    assert_eq!(
        parsed.as_str().split('?').next(),
        Some(GOOGLE_AUTHORIZATION_URL)
    );
    assert_eq!(query.get("response_type").map(|v| v.as_ref()), Some("code"));
    assert_eq!(
        query.get("scope").map(|v| v.as_ref()),
        Some(GOOGLE_MAIL_SCOPE)
    );
    assert_eq!(
        query.get("access_type").map(|v| v.as_ref()),
        Some("offline")
    );
    assert_eq!(query.get("prompt").map(|v| v.as_ref()), Some("consent"));
    assert_eq!(query.get("state").map(|v| v.as_ref()), Some("state-secret"));
    assert_eq!(
        query.get("code_challenge_method").map(|v| v.as_ref()),
        Some("S256")
    );
    assert!(!url.contains("verifier"));
}

/// Ensures the Gmail start result retains the spec's ten-minute pending
/// lifetime as a duration before email persistence converts it to milliseconds.
#[test]
fn gmail_installed_app_start_uses_ten_minute_duration() {
    let client: GoogleOauthClient<String> = GoogleOauthClient::new(BTreeMap::from([(
        "client-id".to_owned(),
        SecretValue::new("client-id"),
    )]));
    let started = client
        .start_gmail_installed_app_auth(GoogleOauthSecretConfig {
            client_id_secret: "client-id",
            client_secret_secret: None,
            refresh_token_secret: None,
        })
        .expect("Gmail authorization starts");
    assert_eq!(started.pending_lifetime, Duration::from_secs(10 * 60));
}

/// Ensures installed-app token responses convert provider seconds into the
/// named result's duration before reaching the email backend boundary.
#[test]
fn installed_app_token_response_retains_access_token_lifetime_as_duration() {
    let finished = parse_installed_app_token_response(
        r#"{"refresh_token":"refresh-token","access_token":"access-token","expires_in":3600}"#,
    )
    .expect("installed-app token response parses");
    assert_eq!(
        finished.access_token_lifetime,
        Some(Duration::from_secs(3600))
    );
}

/// Ensures token exchanges use the exact stored redirect URI and PKCE
/// verifier instead of deriving redirect data from the pasted browser URL.
#[test]
fn installed_app_token_body_contains_verifier_and_exact_redirect_uri() {
    let body = build_installed_app_token_request_body(
        "client-id",
        Some("client-secret"),
        "auth-code",
        "pkce-verifier",
        "http://127.0.0.1:54321/",
    );
    let parsed = form_urlencoded::parse(body.as_bytes()).collect::<BTreeMap<_, _>>();
    assert_eq!(
        parsed.get("grant_type").map(|v| v.as_ref()),
        Some("authorization_code")
    );
    assert_eq!(parsed.get("code").map(|v| v.as_ref()), Some("auth-code"));
    assert_eq!(
        parsed.get("code_verifier").map(|v| v.as_ref()),
        Some("pkce-verifier")
    );
    assert_eq!(
        parsed.get("redirect_uri").map(|v| v.as_ref()),
        Some("http://127.0.0.1:54321/")
    );
}

/// Ensures only the stored loopback redirect shape is accepted and the
/// authorization code is returned without echoing token-like URL values in
/// validation errors.
#[test]
fn parses_installed_app_redirect_url_against_stored_redirect() {
    let redirect = parse_installed_app_redirect_url(
        "http://127.0.0.1:54321/?state=expected&code=secret-code",
        &LoopbackRedirectUri::from_generator("http://127.0.0.1:54321/".to_owned()),
        &OauthState::from_generator("expected".to_owned()),
    )
    .expect("redirect parses");
    assert_eq!(redirect.code.expose_for_provider(), "secret-code");

    let err = parse_installed_app_redirect_url(
        "https://evil.test/callback?state=expected&code=secret-code",
        &LoopbackRedirectUri::from_generator("http://127.0.0.1:54321/".to_owned()),
        &OauthState::from_generator("expected".to_owned()),
    )
    .expect_err("non-loopback rejected");
    assert!(!err.contains("secret-code"));

    let err = parse_installed_app_redirect_url(
        "http://127.0.0.1:54321/?state=wrong&code=secret-code",
        &LoopbackRedirectUri::from_generator("http://127.0.0.1:54321/".to_owned()),
        &OauthState::from_generator("expected".to_owned()),
    )
    .expect_err("wrong state rejected");
    assert!(!err.contains("wrong"));
    assert!(!err.contains("secret-code"));

    for (url, reason) in [
        (
            "http://127.0.0.1:54322/?state=expected&code=secret-code",
            "wrong port",
        ),
        (
            "http://127.0.0.1:54321/callback?state=expected&code=secret-code",
            "wrong path",
        ),
        (
            "http://127.0.0.1:54321/?state=expected&code=secret-code#fragment",
            "fragment",
        ),
        ("http://127.0.0.1:54321/?code=secret-code", "missing state"),
        ("http://127.0.0.1:54321/?state=expected", "missing code"),
    ] {
        let result = parse_installed_app_redirect_url(
            url,
            &LoopbackRedirectUri::from_generator("http://127.0.0.1:54321/".to_owned()),
            &OauthState::from_generator("expected".to_owned()),
        );
        let err = match result {
            Ok(_) => panic!("{reason} should reject"),
            Err(err) => err,
        };
        assert!(!err.contains("secret-code"), "{reason}: {err}");
    }

    let oversized = format!(
        "http://127.0.0.1:54321/?state=expected&code={}",
        "x".repeat(MAX_REDIRECT_URL_CHARS)
    );
    let err = parse_installed_app_redirect_url(
        &oversized,
        &LoopbackRedirectUri::from_generator("http://127.0.0.1:54321/".to_owned()),
        &OauthState::from_generator("expected".to_owned()),
    )
    .expect_err("oversized URL rejected");
    assert!(!err.contains('x'));

    let err = validate_loopback_redirect_uri("http://127.0.0.1:54321/callback")
        .expect_err("stored non-root redirect rejected");
    assert!(err.contains("127.0.0.1"));
}

/// Ensures redirect query validation rejects duplicate or unsafe sensitive
/// parameters without echoing authorization codes or state-like values.
#[test]
fn installed_app_redirect_rejects_duplicate_and_unsafe_query_parameters() {
    for (url, expected_error, reason) in [
        (
            "http://127.0.0.1:54321/?state=expected&state=expected&code=secret-code",
            "Google redirect URL contained duplicate state",
            "duplicate state",
        ),
        (
            "http://127.0.0.1:54321/?state=expected&code=secret-code&code=other-secret",
            "Google redirect URL contained duplicate code",
            "duplicate code",
        ),
        (
            "http://127.0.0.1:54321/?state=expected&error=access_denied&error=invalid_request",
            "Google redirect URL contained duplicate error",
            "duplicate error",
        ),
        (
            "http://127.0.0.1:54321/?state=expected&code=secret%0Acode",
            "Google redirect URL authorization code was invalid",
            "unsafe code",
        ),
    ] {
        let err = parse_installed_app_redirect_url(
            url,
            &LoopbackRedirectUri::from_generator("http://127.0.0.1:54321/".to_owned()),
            &OauthState::from_generator("expected".to_owned()),
        )
        .expect_err(reason);
        assert_eq!(err, expected_error, "{reason}");
        assert!(!err.contains("secret"), "{reason}: {err}");
        assert!(!err.contains("expected"), "{reason}: {err}");
        assert!(!err.contains("invalid_request"), "{reason}: {err}");
    }
}

/// Ensures user-denied Google redirects produce a short actionable error
/// after state validation and without echoing the full pasted URL.
#[test]
fn installed_app_redirect_handles_access_denied() {
    let err = parse_installed_app_redirect_url(
        "http://127.0.0.1:54321/?state=expected&error=access_denied",
        &LoopbackRedirectUri::from_generator("http://127.0.0.1:54321/".to_owned()),
        &OauthState::from_generator("expected".to_owned()),
    )
    .expect_err("denial is reported");
    assert_eq!(err, "Google authorization was denied");
}

/// Ensures OAuth HTTP errors redact exact request secrets before provider
/// response text can reach UI-facing diagnostics.
#[test]
fn oauth_http_errors_redact_submitted_secret_values() {
    let message = format_google_oauth_http_error(
        "finishing Google authorization",
        400,
        r#"{"error":"invalid_grant","error_description":"bad code auth-code-secret verifier pkce-verifier-secret client client-secret"}"#,
        &["auth-code-secret", "pkce-verifier-secret", "client-secret"],
    );
    assert!(message.contains("invalid_grant"));
    assert!(!message.contains("auth-code-secret"));
    assert!(!message.contains("pkce-verifier-secret"));
    assert!(!message.contains("client-secret"));
    assert!(message.contains("<redacted>"));
}

/// Ensures malicious or malformed provider expiry values cannot panic the
/// access-token cache by overflowing `Instant`.
#[test]
fn huge_expires_in_skips_cache_without_panicking() {
    let client: GoogleOauthClient<String> = GoogleOauthClient::new(BTreeMap::new());
    let account_id = "work".to_owned();
    client
        .prime_access_token_cache(
            &account_id,
            AccessToken::from_validated_provider("access-token".to_owned()),
            Some(u64::MAX),
        )
        .expect("huge expiry is ignored");
    assert!(
        client
            .cached_access_token(&account_id)
            .expect("cache remains readable")
            .is_none()
    );
}

/// Ensures a completed installed-app result with an unrepresentable duration
/// takes the same no-cache overflow path as raw provider expiries.
#[test]
fn huge_installed_app_lifetime_skips_cache_without_panicking() {
    let client: GoogleOauthClient<String> = GoogleOauthClient::new(BTreeMap::new());
    let account_id = "work".to_owned();
    client
        .prime_access_token_cache_with_lifetime(
            &account_id,
            AccessToken::from_validated_provider("access-token".to_owned()),
            Some(Duration::from_secs(u64::MAX)),
        )
        .expect("huge lifetime is ignored");
    assert!(
        client
            .cached_access_token(&account_id)
            .expect("cache remains readable")
            .is_none()
    );
}

/// Same-spelled email and calendar accounts must remain separate cache-key
/// types while preserving each client's ordinary hit and invalidation behavior.
#[test]
fn access_token_caches_retain_feature_owned_account_identity() {
    let email_id = EmailAccountId::test("shared");
    let calendar_id = CalendarAccountId::test("shared");
    let email: GoogleOauthClient<EmailAccountId> = GoogleOauthClient::new(BTreeMap::new());
    let calendar: GoogleOauthClient<CalendarAccountId> = GoogleOauthClient::new(BTreeMap::new());

    email
        .prime_access_token_cache(
            &email_id,
            AccessToken::from_validated_provider("email-token".to_owned()),
            Some(3600),
        )
        .expect("email cache prime");
    calendar
        .prime_access_token_cache(
            &calendar_id,
            AccessToken::from_validated_provider("calendar-token".to_owned()),
            Some(3600),
        )
        .expect("calendar cache prime");

    assert_eq!(
        email
            .cached_access_token(&email_id)
            .expect("email cache")
            .as_ref()
            .map(AccessToken::expose_for_provider),
        Some("email-token")
    );
    assert_eq!(
        calendar
            .cached_access_token(&calendar_id)
            .expect("calendar cache")
            .as_ref()
            .map(AccessToken::expose_for_provider),
        Some("calendar-token")
    );

    email
        .invalidate_access_token(&email_id)
        .expect("email invalidation");
    assert!(
        email
            .cached_access_token(&email_id)
            .expect("email cache")
            .is_none()
    );
    assert_eq!(
        calendar
            .cached_access_token(&calendar_id)
            .expect("calendar cache")
            .as_ref()
            .map(AccessToken::expose_for_provider),
        Some("calendar-token")
    );
}
