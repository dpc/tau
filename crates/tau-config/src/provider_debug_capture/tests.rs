use super::{ProviderDebugCaptureClass, ProviderDebugCaptureFilename};

/// Ensures every capture class has one exact canonical compressed basename,
/// parses back to that basename, and rejects the legacy uncompressed form.
#[test]
fn filename_round_trips_every_supported_compressed_class_only() {
    let prompt = tau_proto::AgentPromptId::parse("sp-6").expect("prompt id");
    for (class, expected) in [
        (
            ProviderDebugCaptureClass::HttpSseRequest,
            "123-sp-6-http-sse-request.json.zst",
        ),
        (
            ProviderDebugCaptureClass::WebsocketRequest,
            "123-sp-6-websocket-request.json.zst",
        ),
        (
            ProviderDebugCaptureClass::HttpSseResponse,
            "123-sp-6-http-sse-response.json.zst",
        ),
        (
            ProviderDebugCaptureClass::WebsocketResponse,
            "123-sp-6-websocket-response.json.zst",
        ),
        (
            ProviderDebugCaptureClass::UnknownResponse,
            "123-sp-6-unknown-response.json.zst",
        ),
        (
            ProviderDebugCaptureClass::ResponsesAttemptFailure,
            "123-sp-6-responses-attempt-failure.json.zst",
        ),
        (
            ProviderDebugCaptureClass::CompactHttpFailure,
            "123-sp-6-compact-http-failure.json.zst",
        ),
        (
            ProviderDebugCaptureClass::CacheDiagnostic,
            "123-sp-6-cache-diagnostic.json.zst",
        ),
    ] {
        let filename = ProviderDebugCaptureFilename::new(123, &prompt, class);
        assert_eq!(filename.as_str(), expected);
        assert_eq!(
            ProviderDebugCaptureFilename::parse(expected),
            Some(filename.clone())
        );
        let legacy = expected.strip_suffix(".zst").expect("compressed suffix");
        assert!(
            ProviderDebugCaptureFilename::parse(legacy).is_none(),
            "{legacy} must remain unsupported"
        );
    }
}

/// Ensures unrelated suffix collisions and malformed timestamp/prompt
/// components never enter retention cleanup.
#[test]
fn filename_parser_rejects_unrelated_and_malformed_names() {
    for invalid in [
        "notes-http-sse-request.json",
        "123-prompt-http-sse-request.json",
        "backup-websocket-response.json.zst",
        "-prompt-http-sse-request.json",
        "abc-prompt-http-sse-request.json",
        "123-bad.prompt-http-sse-request.json",
        "123-prompt-unknown-request.json.zst",
        "123-prompt-http-sse-request.json.gz",
        "123-prompt-http-sse-request.json.zst.extra",
        "0123-prompt-http-sse-request.json.zst",
        "340282366920938463463374607431768211456-prompt-http-sse-request.json.zst",
    ] {
        assert!(
            ProviderDebugCaptureFilename::parse(invalid).is_none(),
            "{invalid} must remain unrelated"
        );
    }
    let max = format!("{}-prompt-http-sse-request.json.zst", u128::MAX);
    assert!(ProviderDebugCaptureFilename::parse(&max).is_some());
}
/// Operation filenames use a separate grammar, never synthetic prompt IDs;
/// canonical parsing and class pairing still bound managed cleanup discovery.
#[test]
fn operation_filename_roundtrip_and_class_rejection() {
    let id = tau_proto::CacheOperationId::from_bytes([0xab; 16]);
    let filename = super::ProviderDebugCaptureFilename::cache_operation(17, id);
    assert_eq!(
        super::ProviderDebugCaptureFilename::parse(filename.as_str()),
        Some(filename.clone())
    );
    assert!(filename.as_str().starts_with("17.cache-operation."));
    assert!(
        super::ProviderDebugCaptureFilename::parse(&filename.as_str().replacen("17", "017", 1))
            .is_none()
    );
    assert!(
        super::ProviderDebugCaptureFilename::parse(
            &filename
                .as_str()
                .replace("cache-diagnostic", "websocket-request")
        )
        .is_none()
    );
    assert!(
        super::ProviderDebugCaptureFilename::attributed(
            17,
            &tau_proto::ProviderCaptureAttribution::CacheOperation(id),
            tau_proto::ProviderDebugCaptureClass::WebsocketRequest
        )
        .is_none()
    );
}
