use super::*;

/// Ensures the IMAP XOAUTH2 SASL payload matches Gmail's documented
/// `user=` and bearer-token control-A format exactly.
#[test]
fn xoauth2_payload_uses_gmail_sasl_format() {
    assert_eq!(
        xoauth2_payload("alice@example.com", "access-token"),
        "user=alice@example.com\x01auth=Bearer access-token\x01\x01"
    );
}

/// Ensures the SMTP OAuth path pins lettre to XOAUTH2 rather than allowing
/// the default PLAIN/LOGIN mechanism list for bearer-token credentials.
#[test]
fn smtp_oauth_mechanism_selection_is_xoauth2_only() {
    assert_eq!(smtp_oauth_mechanisms(), vec![Mechanism::Xoauth2]);
}

/// Ensures SMTP diagnostics redact the exact bearer token before they can
/// reach action/tool errors or logs.
#[test]
fn smtp_error_sanitizer_redacts_access_token() {
    let sanitized = sanitized_backend_error_redacting(
        "server rejected bearer ya29.secret-token during auth",
        "ya29.secret-token",
    );
    assert_eq!(sanitized, "server rejected bearer [redacted] during auth");
    assert!(!sanitized.contains("ya29.secret-token"));
}
