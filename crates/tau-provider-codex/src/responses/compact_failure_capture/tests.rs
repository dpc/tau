use reqwest::header::{HeaderMap, HeaderValue};
use sha2::{Digest as _, Sha256};

use super::*;

fn context(enabled: bool) -> CompactFailureCaptureContext {
    CompactFailureCaptureContext {
        session_id: tau_proto::SessionId::parse("session-test").expect("session"),
        agent_prompt_id: Some(tau_proto::AgentPromptId::parse("prompt-test").expect("prompt")),
        enabled,
        credentials: vec![b"credential-value".to_vec()],
        sink: None,
        body_chunk_observer: None,
    }
}

fn captured_record(
    context: &CompactFailureCaptureContext,
    status: u16,
    headers: &HeaderMap,
    body: CapturedBody,
) -> serde_json::Value {
    let mut captured = None;
    context.submit_with(status, headers, body, |capture| captured = Some(capture));
    let capture = captured.expect("enabled capture");
    assert_eq!(
        capture.class(),
        ProviderDebugCaptureClass::CompactHttpFailure
    );
    serde_json::from_slice(capture.json()).expect("capture JSON")
}

/// A complete compact rejection must retain causal status, allowlisted
/// correlation, bounded parsed fields, exact body bytes, and full-body hash
/// while removing only configured credentials.
#[test]
fn complete_failure_capture_preserves_forensic_evidence() {
    let wire_body = br#"{"error":{"code":"bad_request","type":"invalid_request","param":"input","message":"credential-value rejected"}}"#;
    let mut accumulator = BodyCapture::new(MAX_CREDENTIAL_BYTES - 1);
    accumulator.push(wire_body);
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().expect("header"));
    headers.insert("retry-after", "7".parse().expect("header"));
    headers.insert("x-request-id", "request-7".parse().expect("header"));
    headers.insert("request-id", "request-8".parse().expect("header"));
    headers.insert("openai-request-id", "request-9".parse().expect("header"));
    headers.insert(
        "authorization",
        "credential-value".parse().expect("unlisted header"),
    );

    let record = captured_record(&context(true), 400, &headers, accumulator.finish(true));

    assert!(!record.to_string().contains("credential-value"));
    assert_eq!(record["schema_version"], 0);
    assert_eq!(record["backend"]["transport"], "unary_http");
    assert_eq!(record["http"]["status"], 400);
    assert_eq!(
        record["http"]["headers"]["x_request_id"]["utf8"],
        "request-7"
    );
    assert_eq!(record["http"]["headers"]["request_id"]["utf8"], "request-8");
    assert_eq!(
        record["http"]["headers"]["openai_request_id"]["utf8"],
        "request-9"
    );
    assert!(record["http"]["headers"].get("authorization").is_none());
    assert_eq!(record["body"]["decoded_bytes_received"], wire_body.len());
    assert_eq!(record["body"]["complete"], true);
    assert_eq!(record["body"]["truncated"], false);
    assert_eq!(record["body"]["sha256_coverage"], "complete_decoded_body");
    assert_eq!(
        record["body"]["sha256_decoded_received"],
        format!("{:x}", Sha256::digest(wire_body))
    );
    let retained = BASE64_STANDARD
        .decode(
            record["body"]["redacted_decoded_prefix_base64"]
                .as_str()
                .expect("base64"),
        )
        .expect("decode");
    assert!(
        !retained
            .windows(16)
            .any(|window| window == b"credential-value")
    );
    assert!(
        retained
            .windows(21)
            .any(|window| window == b"<redacted-credential>")
    );
    assert_eq!(
        record["body"]["parsed_error"]["code"]["utf8"],
        "bad_request"
    );
    assert_eq!(
        record["body"]["parsed_error"]["message"]["utf8"],
        "<redacted-credential> rejected"
    );
}

/// Crossing the 64-KiB prefix cap must stop retention, hash every byte in the
/// delivered crossing chunk, and state that EOF/full-body coverage is unknown.
#[test]
fn oversized_failure_capture_reports_honest_hash_coverage() {
    let prefix = vec![b'a'; MAX_RETAINED_BODY_BYTES];
    let crossing = vec![b'b'; 257];
    let mut accumulator = BodyCapture::new(MAX_CREDENTIAL_BYTES - 1);
    accumulator.push(&prefix);
    accumulator.push(&crossing);
    assert!(!accumulator.reached_retention_limit());
    let mut delivered = prefix.clone();
    delivered.extend_from_slice(&crossing);

    let record = captured_record(
        &context(true),
        413,
        &HeaderMap::new(),
        accumulator.finish(false),
    );

    assert_eq!(record["body"]["decoded_bytes_received"], delivered.len());
    assert!(
        record["body"]["retained_bytes"]
            .as_u64()
            .expect("retained bytes")
            <= MAX_RETAINED_BODY_BYTES as u64
    );
    assert_eq!(record["body"]["complete"], false);
    assert_eq!(record["body"]["truncated"], true);
    assert_eq!(record["body"]["sha256_coverage"], "decoded_bytes_received");
    assert_eq!(
        record["body"]["sha256_decoded_received"],
        format!("{:x}", Sha256::digest(&delivered))
    );
    assert!(record["body"].get("parsed_error").is_none());
}

/// Debug-disabled prompts must not submit even bounded failure evidence.
#[test]
fn disabled_failure_capture_submits_nothing() {
    let mut submitted = false;
    context(false).submit_with(
        400,
        &HeaderMap::new(),
        BodyCapture::new(MAX_CREDENTIAL_BYTES - 1).finish(true),
        |_| submitted = true,
    );
    assert!(!submitted);
}

/// Header and parsed-message fields must carry exact truncation metadata and
/// never exceed their independent byte ceilings.
#[test]
fn failure_capture_bounds_headers_and_provider_message() {
    let message = "m".repeat(MAX_PARSED_MESSAGE_BYTES + 1);
    let wire_body =
        serde_json::to_vec(&serde_json::json!({"error": {"message": message}})).expect("JSON");
    let mut accumulator = BodyCapture::new(MAX_CREDENTIAL_BYTES - 1);
    accumulator.push(&wire_body);
    let mut headers = HeaderMap::new();
    headers.insert(
        "request-id",
        HeaderValue::from_bytes(&vec![b'r'; MAX_HEADER_BYTES + 1]).expect("header"),
    );

    let record = captured_record(&context(true), 400, &headers, accumulator.finish(true));

    assert_eq!(
        record["http"]["headers"]["request_id"]["retained_bytes"],
        MAX_HEADER_BYTES
    );
    assert_eq!(record["http"]["headers"]["request_id"]["truncated"], true);
    assert_eq!(
        record["body"]["parsed_error"]["message"]["retained_bytes"],
        MAX_PARSED_MESSAGE_SCALARS
    );
    assert_eq!(
        record["body"]["parsed_error"]["message"]["retained_unicode_scalars"],
        MAX_PARSED_MESSAGE_SCALARS
    );
    assert_eq!(record["body"]["parsed_error"]["message"]["truncated"], true);
}

/// A credential split across the 64-KiB evidence boundary must be removed as
/// one secret before the redacted prefix cap is applied.
#[test]
fn credential_crossing_prefix_boundary_is_fully_redacted() {
    let credential = b"credential-value";
    let mut wire_body = vec![b'x'; MAX_UNREDACTED_PREFIX_BYTES - credential.len() / 2];
    let split = credential.len() / 2;
    wire_body.extend_from_slice(&credential[..split]);
    let capture_context = context(true);
    let mut accumulator = capture_context.body_capture();
    accumulator.push(&wire_body);
    accumulator.push(&credential[split..]);
    accumulator.push(b"tail-padding-that-fills-lookahead");

    let record = captured_record(
        &capture_context,
        400,
        &HeaderMap::new(),
        accumulator.finish(false),
    );
    let retained = BASE64_STANDARD
        .decode(
            record["body"]["redacted_decoded_prefix_base64"]
                .as_str()
                .expect("base64"),
        )
        .expect("decode");

    assert!(
        !retained
            .windows(credential.len())
            .any(|window| window == credential)
    );
    assert!(retained.len() <= MAX_RETAINED_BODY_BYTES);
    assert!(
        retained
            .windows(REDACTION.len())
            .any(|window| window == REDACTION)
    );
    assert!(!retained.ends_with(&credential[..split]));
}

/// An incomplete below-cap body ending in a credential prefix must withhold the
/// ambiguous suffix and mark that private evidence as truncated.
#[test]
fn incomplete_short_body_withholds_credential_prefix() {
    let capture_context = context(true);
    let credential_prefix = b"credential-val";
    let mut accumulator = capture_context.body_capture();
    accumulator.push(b"diagnostic:");
    accumulator.push(credential_prefix);

    let record = captured_record(
        &capture_context,
        400,
        &HeaderMap::new(),
        accumulator.finish(false),
    );
    let retained = BASE64_STANDARD
        .decode(
            record["body"]["redacted_decoded_prefix_base64"]
                .as_str()
                .expect("base64"),
        )
        .expect("decode");

    assert_eq!(retained, b"diagnostic:");
    assert_eq!(record["body"]["truncated"], true);
}

/// Parsed provider fields accept only strings; null, numbers, and containers
/// must not masquerade as bounded diagnostic prose.
#[test]
fn parsed_provider_fields_reject_non_strings() {
    let wire_body = serde_json::to_vec(&serde_json::json!({
        "error": {
            "code": 42,
            "type": null,
            "param": ["input"],
            "message": {"text": "not prose"}
        }
    }))
    .expect("JSON");
    let mut accumulator = BodyCapture::new(MAX_CREDENTIAL_BYTES - 1);
    accumulator.push(&wire_body);

    let record = captured_record(
        &context(true),
        400,
        &HeaderMap::new(),
        accumulator.finish(true),
    );

    assert!(record["body"].get("parsed_error").is_none());
}
