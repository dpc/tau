use super::*;

/// Ensures standard delta and HTTP-date hints survive transport boundaries.
#[test]
fn parses_retry_after_forms() {
    let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    assert_eq!(
        parse_retry_after("123", now),
        Some(Duration::from_secs(123))
    );
    let date = httpdate::fmt_http_date(now + Duration::from_secs(321));
    assert_eq!(
        parse_retry_after(&date, now),
        Some(Duration::from_secs(321))
    );
    assert_eq!(parse_retry_after("-1", now), None);
}

/// Ensures reset fields remain usable even when providers change nesting.
#[test]
fn parses_nested_reset_fields() {
    let now = UNIX_EPOCH + Duration::from_secs(100);
    assert_eq!(
        parse_json_reset_hint(r#"{"event":{"error":{"resets_at":160}}}"#, now),
        Some(Duration::from_secs(60))
    );
    assert_eq!(
        parse_json_reset_hint(r#"{"error":{"resets_in_seconds":90}}"#, now),
        Some(Duration::from_secs(90))
    );
}

/// Covers trusted timing-hint boundaries so malformed, negative, missing,
/// past, and overflowing inputs cannot fabricate a future reset.
#[test]
fn rejects_untrusted_reset_hints_and_saturates_past_dates() {
    let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let past = httpdate::fmt_http_date(now - Duration::from_secs(60));
    for value in ["", "nonsense", "-1", "18446744073709551616"] {
        assert_eq!(parse_retry_after(value, now), None, "{value:?}");
    }
    assert_eq!(parse_retry_after(&past, now), Some(Duration::ZERO));
    for body in [
        "{}",
        r#"{"resets_in_seconds":-1}"#,
        r#"{"resets_in_seconds":"60"}"#,
        r#"{"resets_at":-1}"#,
        r#"{"resets_at":18446744073709551616}"#,
        "not-json",
    ] {
        assert_eq!(parse_json_reset_hint(body, now), None, "{body}");
    }
    assert_eq!(
        parse_json_reset_hint(r#"{"resets_at":1}"#, now),
        Some(Duration::ZERO),
        "past structured dates saturate without underflow"
    );
}

/// Locks the shared-versus-prompt-local retry-class policy used by the
/// scheduler reference model and provider runtime.
#[test]
fn retry_class_scope_matrix_is_stable() {
    for class in [
        RetryClass::Throttle,
        RetryClass::UsageWindow,
        RetryClass::Account,
        RetryClass::Auth,
    ] {
        assert!(class.shares_cooldown(), "{class:?} is provider scoped");
    }
    for class in [
        RetryClass::Transport,
        RetryClass::Overload,
        RetryClass::Unknown,
    ] {
        assert!(!class.shares_cooldown(), "{class:?} is prompt local");
    }
}

/// Ensures billing and unfamiliar provider codes remain retryable classes.
#[test]
fn classifies_account_and_unknown_errors() {
    assert_eq!(
        classify_error_code("insufficient_quota"),
        RetryClass::Account
    );
    assert_eq!(
        classify_error_code("usage_limit_s_reached"),
        RetryClass::Unknown
    );
}

/// Ensures the current Codex overload identifier uses the short, prompt-local
/// overload cadence rather than the persistent unknown-failure cadence.
#[test]
fn classifies_current_codex_overload_identifier() {
    assert_eq!(
        classify_error_code("server_is_overloaded"),
        RetryClass::Overload
    );
}
