use super::CanonicalIdentifierFamily;

/// A decisive later context identifier must outrank an earlier transient one.
#[test]
fn context_identifier_has_family_wide_precedence() {
    let value = serde_json::json!({
        "code": "server_error",
        "response": {"error": {"type": "context_length_exceeded"}}
    });
    assert_eq!(
        CanonicalIdentifierFamily::from_http_envelope(&value).classified(),
        Some("context_length_exceeded")
    );
}

/// A known later retry identifier must outrank earlier unknown evidence.
#[test]
fn known_transient_outranks_unknown_identifier() {
    let value = serde_json::json!({
        "code": "new_provider_code",
        "error": {"type": "rate_limit_exceeded"}
    });
    assert_eq!(
        CanonicalIdentifierFamily::from_http_envelope(&value).classified(),
        Some("rate_limit_exceeded")
    );
}

/// Stream extraction accepts only the reviewed provider-specific metadata path.
#[test]
fn stream_extractor_does_not_search_arbitrary_metadata() {
    let value = serde_json::json!({
        "code": "unknown",
        "metadata": {
            "nested": {"error_type": "context_length_exceeded"},
            "error_type": "rate_limit_exceeded"
        }
    });
    assert_eq!(
        CanonicalIdentifierFamily::from_stream_error(value.as_object().expect("object"))
            .classified(),
        Some("rate_limit_exceeded")
    );
}
