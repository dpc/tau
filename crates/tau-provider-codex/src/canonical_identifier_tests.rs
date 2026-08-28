use super::CanonicalIdentifierFamily;

/// A later context identifier must outrank an earlier transient identifier
/// because retry cannot make an unchanged oversized request succeed.
#[test]
fn context_identifier_has_family_wide_precedence() {
    let value = serde_json::json!({
        "code": "server_error",
        "response": {"error": {"type": "context_length_exceeded"}}
    });
    assert_eq!(
        CanonicalIdentifierFamily::from_value(&value).classified(),
        Some("context_length_exceeded")
    );
}

/// A later known transient identifier must outrank earlier unknown evidence
/// while preserving canonical envelope order among known identifiers.
#[test]
fn known_transient_outranks_earlier_unknown_identifier() {
    let value = serde_json::json!({
        "code": "new_unclassified_code",
        "error": {"type": "rate_limit_exceeded"}
    });
    assert_eq!(
        CanonicalIdentifierFamily::from_value(&value).classified(),
        Some("rate_limit_exceeded")
    );
}

/// Streaming event `type` belongs to the compact/ordinary event language and
/// must not hide the nested canonical provider error identifier.
#[test]
fn provider_event_type_is_not_an_error_identifier() {
    let value = serde_json::json!({
        "type": "response.failed",
        "response": {"error": {"code": "invalid_prompt"}}
    });
    assert_eq!(
        CanonicalIdentifierFamily::from_provider_event(&value).classified(),
        Some("invalid_prompt")
    );
}
