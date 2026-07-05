use super::*;

/// The stream fingerprint must include both API base and token so unrelated
/// bots or test Bot API endpoints do not contend for the same advisory lock.
#[test]
fn stream_fingerprint_identifies_api_base_and_bot_token() {
    let baseline = StreamIdentity::new("https://api.telegram.org", "token-a").fingerprint();

    assert_ne!(
        baseline,
        StreamIdentity::new("https://api.telegram.org", "token-b").fingerprint()
    );
    assert_ne!(
        baseline,
        StreamIdentity::new("https://telegram.example", "token-a").fingerprint()
    );
}

/// Redaction is shared by HTTP diagnostics and future owner diagnostics so
/// Telegram bot tokens never appear in user-visible errors.
#[test]
fn stream_identity_redacts_its_bot_token() {
    let identity = StreamIdentity::new("https://api.telegram.org", "secret-token");

    assert_eq!(
        identity.redact_token("bad secret-token in response"),
        "bad <redacted> in response"
    );
}
