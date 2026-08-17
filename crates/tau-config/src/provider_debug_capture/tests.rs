use super::{ProviderDebugCaptureClass, ProviderDebugCaptureFilename};

/// Ensures construction and parsing agree for every supported compressed
/// capture class, while legacy uncompressed names are rejected.
#[test]
fn filename_round_trips_every_supported_compressed_class_only() {
    let prompt = tau_proto::AgentPromptId::parse("sp-6").expect("prompt id");
    for class in [
        ProviderDebugCaptureClass::HttpSseRequest,
        ProviderDebugCaptureClass::WebsocketRequest,
        ProviderDebugCaptureClass::HttpSseResponse,
        ProviderDebugCaptureClass::WebsocketResponse,
        ProviderDebugCaptureClass::UnknownResponse,
        ProviderDebugCaptureClass::ResponsesAttemptFailure,
        ProviderDebugCaptureClass::CompactHttpFailure,
    ] {
        let filename = ProviderDebugCaptureFilename::new(123, &prompt, class);
        assert_eq!(
            ProviderDebugCaptureFilename::parse(filename.as_str()),
            Some(filename.clone())
        );
        let legacy = filename
            .as_str()
            .strip_suffix(".zst")
            .expect("compressed suffix");
        assert!(
            ProviderDebugCaptureFilename::parse(legacy).is_none(),
            "{legacy} must remain unsupported"
        );
    }
}

/// Compact HTTP failure artifacts need a distinct stable basename so retention
/// cleanup and forensic tooling cannot confuse them with normalized responses.
#[test]
fn compact_http_failure_has_distinct_compressed_filename() {
    let prompt = tau_proto::AgentPromptId::parse("compact-7").expect("prompt id");
    let filename = ProviderDebugCaptureFilename::new(
        123,
        &prompt,
        ProviderDebugCaptureClass::CompactHttpFailure,
    );
    assert_eq!(
        filename.as_str(),
        "123-compact-7-compact-http-failure.json.zst"
    );
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
