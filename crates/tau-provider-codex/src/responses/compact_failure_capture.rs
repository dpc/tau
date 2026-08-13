use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use reqwest::header::HeaderMap;
use serde::Serialize;
use tau_provider::debug_capture_writer::{ProviderDebugCapture, ProviderDebugCaptureClass};

use crate::LOG_TARGET;
use crate::common::PromptPayload;
use crate::responses::ResponsesConfig;

mod body_capture;
#[cfg(test)]
use body_capture::MAX_UNREDACTED_PREFIX_BYTES;
pub(in crate::responses) use body_capture::{BodyCapture, CapturedBody};
use body_capture::{MAX_CREDENTIAL_BYTES, MAX_RETAINED_BODY_BYTES};
const MAX_HEADER_BYTES: usize = 1024;
const MAX_CONTENT_HEADER_BYTES: usize = 256;
const MAX_PARSED_IDENTIFIER_BYTES: usize = 1024;
const MAX_PARSED_IDENTIFIER_SCALARS: usize = 256;
const MAX_PARSED_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_PARSED_MESSAGE_SCALARS: usize = 4096;
const REDACTION: &[u8] = b"<redacted-credential>";

/// Prompt-owned authority and known credentials for one private failure
/// capture.
#[derive(Clone)]
pub(super) struct CompactFailureCaptureContext {
    /// Durable session that owns the private artifact.
    session_id: tau_proto::SessionId,
    /// Prompt correlated with the compact operation.
    agent_prompt_id: Option<tau_proto::AgentPromptId>,
    /// Whether durable provider diagnostics are enabled for this prompt.
    enabled: bool,
    /// Exact credential byte strings removed from retained evidence.
    credentials: Vec<Vec<u8>>,
    /// Test-only sink that observes production boundary submissions.
    #[cfg(test)]
    sink: Option<std::sync::Arc<std::sync::Mutex<Vec<ProviderDebugCapture>>>>,
    /// Test-only callback fired after client-side body consumption.
    #[cfg(test)]
    body_chunk_observer: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl CompactFailureCaptureContext {
    /// Snapshot capture authority and exact credential redaction inputs.
    pub(super) fn new(
        agent_prompt_id: &str,
        config: &ResponsesConfig,
        request: &PromptPayload<'_>,
    ) -> Self {
        let configured_credentials = [Some(config.api_key.as_str()), config.account_id.as_deref()]
            .into_iter()
            .flatten()
            .filter(|credential| !credential.is_empty())
            .map(|credential| credential.as_bytes().to_vec())
            .collect::<Vec<_>>();
        let credential_too_long = configured_credentials
            .iter()
            .any(|credential| MAX_CREDENTIAL_BYTES < credential.len());
        let mut credentials = configured_credentials
            .into_iter()
            .filter(|credential| credential.len() <= MAX_CREDENTIAL_BYTES)
            .collect::<Vec<_>>();
        credentials.sort_by_key(|credential| std::cmp::Reverse(credential.len()));
        Self {
            session_id: request.session_id.clone(),
            agent_prompt_id: tau_proto::AgentPromptId::parse(agent_prompt_id).ok(),
            enabled: request.debug_provider_requests && !credential_too_long,
            credentials,
            #[cfg(test)]
            sink: None,
            #[cfg(test)]
            body_chunk_observer: None,
        }
    }

    /// Install an injected sink for a production-boundary regression.
    #[cfg(test)]
    pub(super) fn with_test_sink(
        mut self,
        sink: std::sync::Arc<std::sync::Mutex<Vec<ProviderDebugCapture>>>,
    ) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Install a callback that observes client-side body consumption.
    #[cfg(test)]
    pub(super) fn with_test_body_chunk_observer(
        mut self,
        observer: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        self.body_chunk_observer = Some(observer);
        self
    }

    /// Notify a test after one chunk is counted, hashed, and retained.
    #[cfg(test)]
    pub(super) fn observe_test_body_chunk(&self) {
        if let Some(observer) = &self.body_chunk_observer {
            observer();
        }
    }

    /// Submit one bounded zstd-backed artifact through the shared Provider API.
    pub(super) fn submit(&self, status: u16, headers: &HeaderMap, body: CapturedBody) {
        #[cfg(test)]
        if let Some(sink) = self.sink.clone() {
            self.submit_with(status, headers, body, move |capture| {
                sink.lock()
                    .expect("compact failure test sink")
                    .push(capture);
            });
            return;
        }
        self.submit_with(
            status,
            headers,
            body,
            tau_provider::debug_capture_writer::submit_provider_debug_capture,
        );
    }

    /// Build body accounting with enough lookahead for any configured
    /// credential.
    pub(super) fn body_capture(&self) -> BodyCapture {
        BodyCapture::new(
            self.credentials
                .iter()
                .map(Vec::len)
                .max()
                .unwrap_or(1)
                .saturating_sub(1),
        )
    }

    fn submit_with(
        &self,
        status: u16,
        headers: &HeaderMap,
        body: CapturedBody,
        submit: impl FnOnce(ProviderDebugCapture),
    ) {
        if !self.enabled {
            return;
        }
        let Some(agent_prompt_id) = self.agent_prompt_id.clone() else {
            tracing::warn!(
                target: LOG_TARGET,
                "invalid compact failure capture prompt id; dropping capture"
            );
            return;
        };
        let max_credential_overlap = self
            .credentials
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(1)
            .saturating_sub(1);
        let withheld_suffix = if body.complete {
            0
        } else {
            credential_prefix_suffix_len(&body.retained, &self.credentials)
        };
        let safe_input = &body.retained[..body.retained.len().saturating_sub(withheld_suffix)];
        let redacted = redact_credentials(safe_input, &self.credentials);
        let evidence_limit = if body.complete || body.retention_limit_reached {
            MAX_RETAINED_BODY_BYTES
        } else {
            MAX_RETAINED_BODY_BYTES.saturating_sub(max_credential_overlap)
        };
        let redacted_prefix_len = floor_char_boundary_if_utf8(&redacted, evidence_limit);
        let redacted_prefix = &redacted[..redacted_prefix_len];
        let parsed = ParsedProviderError::from_body(&redacted, body.complete);
        let record = CompactHttpFailureRecord {
            schema_version: 0,
            capture_kind: "compact_http_failure",
            session_id: &self.session_id,
            agent_prompt_id: &agent_prompt_id,
            operation: "compact",
            backend: CaptureBackend {
                kind: "responses",
                transport: "unary_http",
            },
            http: CaptureHttp {
                status,
                headers: CapturedHeaders::new(headers, &self.credentials),
            },
            body: CaptureBody {
                decoded_bytes_received: body.decoded_bytes_received,
                retained_bytes: u64::try_from(redacted_prefix.len()).unwrap_or(u64::MAX),
                complete: body.complete,
                truncated: body.truncated
                    || 0 < withheld_suffix
                    || redacted_prefix.len() < redacted.len(),
                redacted_prefix_truncated: redacted_prefix.len() < redacted.len(),
                sha256_decoded_received: hex_digest(&body.sha256_decoded_received),
                sha256_coverage: if body.complete {
                    "complete_decoded_body"
                } else {
                    "decoded_bytes_received"
                },
                redacted_decoded_prefix_base64: BASE64_STANDARD.encode(redacted_prefix),
                parsed_error: parsed,
            },
        };
        let Ok(json) = serde_json::to_vec_pretty(&record) else {
            tracing::warn!(
                target: LOG_TARGET,
                "failed to serialize compact HTTP failure capture"
            );
            return;
        };
        submit(ProviderDebugCapture::new(
            self.session_id.clone(),
            agent_prompt_id,
            ProviderDebugCaptureClass::CompactHttpFailure,
            json,
        ));
    }
}

/// Complete versioned private failure record serialized before zstd transport.
#[derive(Serialize)]
struct CompactHttpFailureRecord<'a> {
    /// Version of this private diagnostic schema.
    schema_version: u8,
    /// Stable discriminator for forensic tooling.
    capture_kind: &'static str,
    /// Durable session attribution.
    session_id: &'a tau_proto::SessionId,
    /// Compact prompt attribution.
    agent_prompt_id: &'a tau_proto::AgentPromptId,
    /// Provider operation.
    operation: &'static str,
    /// Backend and transport facts.
    backend: CaptureBackend,
    /// HTTP status and bounded headers.
    http: CaptureHttp,
    /// Bounded body evidence.
    body: CaptureBody,
}

/// Provider backend and transport facts for one compact failure.
#[derive(Serialize)]
struct CaptureBackend {
    /// Provider protocol family.
    kind: &'static str,
    /// Concrete compact transport.
    transport: &'static str,
}

/// HTTP status and closed allowlisted headers.
#[derive(Serialize)]
struct CaptureHttp {
    /// Non-success HTTP status.
    status: u16,
    /// Closed allowlist of response headers.
    headers: CapturedHeaders,
}

/// Closed set of response headers retained for forensic correlation.
#[derive(Serialize)]
struct CapturedHeaders {
    /// Response media type.
    content_type: Option<BoundedBytes>,
    /// Provider retry instruction.
    retry_after: Option<BoundedBytes>,
    /// Common generic request correlation header.
    request_id: Option<BoundedBytes>,
    /// OpenAI request correlation header.
    openai_request_id: Option<BoundedBytes>,
    /// Alternate OpenAI-compatible request correlation header.
    x_request_id: Option<BoundedBytes>,
}

impl CapturedHeaders {
    fn new(headers: &HeaderMap, credentials: &[Vec<u8>]) -> Self {
        Self {
            content_type: bounded_header(
                headers,
                "content-type",
                MAX_CONTENT_HEADER_BYTES,
                credentials,
            ),
            retry_after: bounded_header(
                headers,
                "retry-after",
                MAX_CONTENT_HEADER_BYTES,
                credentials,
            ),
            request_id: bounded_header(headers, "request-id", MAX_HEADER_BYTES, credentials),
            openai_request_id: bounded_header(
                headers,
                "openai-request-id",
                MAX_HEADER_BYTES,
                credentials,
            ),
            x_request_id: bounded_header(headers, "x-request-id", MAX_HEADER_BYTES, credentials),
        }
    }
}

/// One byte-preserving bounded header or parsed provider field.
#[derive(Serialize)]
struct BoundedBytes {
    /// Original value size before credential redaction.
    original_bytes: u64,
    /// Retained redacted value size.
    retained_bytes: u64,
    /// Original scalar count when the source was provider text.
    #[serde(skip_serializing_if = "Option::is_none")]
    original_unicode_scalars: Option<u64>,
    /// Retained scalar count when the source was provider text.
    #[serde(skip_serializing_if = "Option::is_none")]
    retained_unicode_scalars: Option<u64>,
    /// Whether the redacted value exceeded its field bound.
    truncated: bool,
    /// Exact retained bytes independent of UTF-8 validity.
    base64: String,
    /// Convenience text when the retained bytes are valid UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    utf8: Option<String>,
}

impl BoundedBytes {
    fn new(value: &[u8], max_bytes: usize, credentials: &[Vec<u8>]) -> Self {
        let redacted = redact_credentials(value, credentials);
        let retained_len = floor_char_boundary_if_utf8(&redacted, max_bytes);
        let retained = &redacted[..retained_len];
        Self {
            original_bytes: u64::try_from(value.len()).unwrap_or(u64::MAX),
            retained_bytes: u64::try_from(retained.len()).unwrap_or(u64::MAX),
            original_unicode_scalars: None,
            retained_unicode_scalars: None,
            truncated: retained.len() < redacted.len(),
            base64: BASE64_STANDARD.encode(retained),
            utf8: std::str::from_utf8(retained).ok().map(str::to_owned),
        }
    }

    fn text(value: &str, max_bytes: usize, max_scalars: usize) -> Self {
        let byte_limit = floor_char_boundary_if_utf8(value.as_bytes(), max_bytes);
        let scalar_limit = value
            .char_indices()
            .nth(max_scalars)
            .map_or(value.len(), |(index, _)| index);
        let retained_len = byte_limit.min(scalar_limit);
        let retained = &value[..retained_len];
        Self {
            original_bytes: u64::try_from(value.len()).unwrap_or(u64::MAX),
            retained_bytes: u64::try_from(retained.len()).unwrap_or(u64::MAX),
            original_unicode_scalars: Some(
                u64::try_from(value.chars().count()).unwrap_or(u64::MAX),
            ),
            retained_unicode_scalars: Some(
                u64::try_from(retained.chars().count()).unwrap_or(u64::MAX),
            ),
            truncated: retained.len() < value.len(),
            base64: BASE64_STANDARD.encode(retained),
            utf8: Some(retained.to_owned()),
        }
    }
}

/// Raw and parsed evidence from the compact response body.
#[derive(Serialize)]
struct CaptureBody {
    /// Bytes delivered by the HTTP transport.
    decoded_bytes_received: u64,
    /// Redacted prefix bytes retained in this record.
    retained_bytes: u64,
    /// Whether EOF proved complete-body coverage.
    complete: bool,
    /// Whether delivered bytes were omitted from the retained prefix.
    truncated: bool,
    /// Whether credential replacement expansion exceeded the stored prefix cap.
    redacted_prefix_truncated: bool,
    /// SHA-256 of exactly `decoded_bytes_received` decoded bytes.
    sha256_decoded_received: String,
    /// Closed statement of digest coverage.
    sha256_coverage: &'static str,
    /// Credential-redacted retained bytes.
    redacted_decoded_prefix_base64: String,
    /// Bounded common provider error fields, when complete JSON exposed them.
    #[serde(skip_serializing_if = "Option::is_none")]
    parsed_error: Option<ParsedProviderError>,
}

/// Common bounded fields parsed from a complete provider error object.
#[derive(Serialize)]
struct ParsedProviderError {
    /// Provider error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<BoundedBytes>,
    /// Provider error type.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    ty: Option<BoundedBytes>,
    /// Provider error parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    param: Option<BoundedBytes>,
    /// Provider-authored diagnostic text.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<BoundedBytes>,
}

impl ParsedProviderError {
    fn from_body(body: &[u8], complete: bool) -> Option<Self> {
        if !complete {
            return None;
        }
        let value: serde_json::Value = serde_json::from_slice(body).ok()?;
        let error = value.get("error").unwrap_or(&value);
        let parsed = Self {
            code: bounded_json_field(
                error.get("code"),
                MAX_PARSED_IDENTIFIER_BYTES,
                MAX_PARSED_IDENTIFIER_SCALARS,
            ),
            ty: bounded_json_field(
                error.get("type"),
                MAX_PARSED_IDENTIFIER_BYTES,
                MAX_PARSED_IDENTIFIER_SCALARS,
            ),
            param: bounded_json_field(
                error.get("param"),
                MAX_PARSED_IDENTIFIER_BYTES,
                MAX_PARSED_IDENTIFIER_SCALARS,
            ),
            message: bounded_json_field(
                error.get("message"),
                MAX_PARSED_MESSAGE_BYTES,
                MAX_PARSED_MESSAGE_SCALARS,
            ),
        };
        (parsed.code.is_some()
            || parsed.ty.is_some()
            || parsed.param.is_some()
            || parsed.message.is_some())
        .then_some(parsed)
    }
}

fn bounded_json_field(
    value: Option<&serde_json::Value>,
    max_bytes: usize,
    max_scalars: usize,
) -> Option<BoundedBytes> {
    Some(BoundedBytes::text(value?.as_str()?, max_bytes, max_scalars))
}

fn bounded_header(
    headers: &HeaderMap,
    name: &'static str,
    max_bytes: usize,
    credentials: &[Vec<u8>],
) -> Option<BoundedBytes> {
    headers
        .get(name)
        .map(|value| BoundedBytes::new(value.as_bytes(), max_bytes, credentials))
}

fn redact_credentials(value: &[u8], credentials: &[Vec<u8>]) -> Vec<u8> {
    credentials
        .iter()
        .fold(value.to_vec(), |value, credential| {
            replace_bytes(&value, credential, REDACTION)
        })
}

fn replace_bytes(value: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return value.to_vec();
    }
    let mut output = Vec::with_capacity(value.len());
    let mut cursor = 0;
    while let Some(index) = value[cursor..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let index = cursor + index;
        output.extend_from_slice(&value[cursor..index]);
        output.extend_from_slice(replacement);
        cursor = index + needle.len();
    }
    output.extend_from_slice(&value[cursor..]);
    output
}

fn credential_prefix_suffix_len(value: &[u8], credentials: &[Vec<u8>]) -> usize {
    credentials
        .iter()
        .flat_map(|credential| {
            (1..credential.len()).filter(move |length| {
                *length <= value.len() && value.ends_with(&credential[..*length])
            })
        })
        .max()
        .unwrap_or(0)
}

fn floor_char_boundary_if_utf8(value: &[u8], max_bytes: usize) -> usize {
    let limit = value.len().min(max_bytes);
    let Ok(text) = std::str::from_utf8(value) else {
        return limit;
    };
    (0..=limit)
        .rev()
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or(0)
}

fn hex_digest(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests;
