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

/// llama.cpp's exact typed overflow must outrank an earlier known transient
/// identifier so provider capacity feedback reaches context recovery.
#[test]
fn llama_cpp_context_identifier_has_family_wide_precedence() {
    let value = serde_json::json!({
        "code": "server_error",
        "error": {"type": "rate_limit_exceeded"},
        "response": {"error": {"type": "exceed_context_size_error"}}
    });
    assert_eq!(
        CanonicalIdentifierFamily::from_http_envelope(&value).classified(),
        Some("exceed_context_size_error")
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
