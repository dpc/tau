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
