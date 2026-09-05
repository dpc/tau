//! Provider context and transcript item support types.
//!
//! Semantic fields and provider replay sidecars follow
//! `SPEC-tau-proto-provider-data`.

mod arc_bytes;
mod url_citation;
#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fmt::Write as _;
use std::sync::Arc;

use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
pub use url_citation::UrlCitation;

use crate::events::{ProviderBackend, ToolFormat, ToolType};
use crate::{CborValue, ProviderTokenUsage, ToolCallId, ToolName};

// ---------------------------------------------------------------------------
// Item-based conversation types
// ---------------------------------------------------------------------------

/// Role of a participant in one message item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRole {
    /// System-level instructions.
    System,
    /// Developer-level instructions.
    Developer,
    /// User-authored message content.
    User,
    /// Assistant-authored message content.
    Assistant,
}

/// One content part inside a message item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Plain UTF-8 text content.
    Text {
        /// Text body for this content part.
        text: String,
    },
    /// Exact local-compactor narrative retained with its harness-authenticated
    /// synthetic-summary provenance.
    ///
    /// Provider projection treats this as ordinary text. The discriminator is
    /// durable prompt-assembly authority and is never inferred from `text`.
    SyntheticCompactionSummary {
        /// Raw accepted local-compactor narrative.
        text: String,
    },
    /// Harness-authenticated internal text. Provider projection alone frames
    /// this variant; text spelling never establishes this authority.
    HarnessInternalText {
        /// Raw harness-authored body.
        text: String,
    },
    /// Bounded semantic URL citation attached to an assistant text part.
    ///
    /// This metadata never contributes provider-visible narrative text. The raw
    /// Responses sidecar remains the exact replay authority.
    UrlCitation {
        /// Validated bounded citation value.
        citation: UrlCitation,
    },
    /// One or more provider URL-citation annotations were malformed or unsafe.
    CitationMetadataInvalid,
}

/// The outer transcript family of one opaque provider-owned item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpaqueProviderItemKind {
    /// A provider reasoning item.
    Reasoning,
    /// A provider compaction item.
    Compaction,
    /// A provider item whose family Tau does not understand.
    Unknown,
}

/// Why an opaque provider item failed canonical validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpaqueProviderItemError {
    /// The required raw JSON is malformed.
    MalformedRawJson,
    /// The raw JSON and structured value represent different semantic values.
    SemanticMismatch,
    /// The provider item's `type` does not match its outer transcript family.
    KindMismatch {
        /// Outer transcript family required by the caller.
        expected: OpaqueProviderItemKind,
        /// Provider `type` found in the raw JSON, when it was a string.
        actual_type: Option<String>,
    },
}

impl fmt::Display for OpaqueProviderItemError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedRawJson => formatter.write_str("opaque provider raw JSON is malformed"),
            Self::SemanticMismatch => {
                formatter.write_str("opaque provider raw JSON contradicts its structured value")
            }
            Self::KindMismatch {
                expected,
                actual_type,
            } => write!(
                formatter,
                "opaque provider item type {actual_type:?} does not match outer kind {expected:?}"
            ),
        }
    }
}

impl std::error::Error for OpaqueProviderItemError {}

/// Opaque provider-owned payload preserved without semantic authority.
#[derive(Clone, Debug, PartialEq)]
pub struct OpaqueProviderItem {
    /// Parsed provider item for semantic inspection and replay.
    value: CborValue,
    /// Raw provider item JSON used for cache-identity-preserving replay.
    ///
    /// This sidecar is provider-visible syntax only. Consumers that need to
    /// inspect, validate, or make semantic decisions must use [`Self::value`].
    raw_json: String,
}

impl OpaqueProviderItem {
    /// Validates matching structured and raw representations.
    pub fn try_new(
        value: CborValue,
        raw_json: impl Into<String>,
    ) -> Result<Self, OpaqueProviderItemError> {
        let raw_json = raw_json.into();
        let parsed: serde_json::Value = serde_json::from_str(&raw_json)
            .map_err(|_| OpaqueProviderItemError::MalformedRawJson)?;
        if !json_cbor_semantically_equal(&crate::json_to_cbor(&parsed), &value) {
            return Err(OpaqueProviderItemError::SemanticMismatch);
        }
        Ok(Self { value, raw_json })
    }

    /// Parses raw provider JSON into its canonical structured representation.
    pub fn from_raw_json(raw_json: impl Into<String>) -> Result<Self, OpaqueProviderItemError> {
        let raw_json = raw_json.into();
        let parsed: serde_json::Value = serde_json::from_str(&raw_json)
            .map_err(|_| OpaqueProviderItemError::MalformedRawJson)?;
        Ok(Self {
            value: crate::json_to_cbor(&parsed),
            raw_json,
        })
    }

    /// Returns the parsed provider item used for semantic inspection.
    #[must_use]
    pub fn value(&self) -> &CborValue {
        &self.value
    }

    /// Returns the exact raw provider JSON used for replay.
    #[must_use]
    pub fn raw_json(&self) -> &str {
        &self.raw_json
    }

    /// Validates that the provider `type` matches an outer transcript family.
    pub fn validate_kind(
        &self,
        expected: OpaqueProviderItemKind,
    ) -> Result<(), OpaqueProviderItemError> {
        let parsed: serde_json::Value = serde_json::from_str(&self.raw_json)
            .map_err(|_| OpaqueProviderItemError::MalformedRawJson)?;
        let actual_type = parsed
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let matches = match expected {
            OpaqueProviderItemKind::Reasoning => actual_type.as_deref() == Some("reasoning"),
            OpaqueProviderItemKind::Compaction => actual_type.as_deref() == Some("compaction"),
            OpaqueProviderItemKind::Unknown => actual_type
                .as_deref()
                .is_some_and(|item_type| !matches!(item_type, "reasoning" | "compaction")),
        };
        if matches {
            Ok(())
        } else {
            Err(OpaqueProviderItemError::KindMismatch {
                expected,
                actual_type,
            })
        }
    }
}

fn json_cbor_semantically_equal(left: &CborValue, right: &CborValue) -> bool {
    match (left, right) {
        (CborValue::Array(left), CborValue::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_cbor_semantically_equal(left, right))
        }
        (CborValue::Map(left), CborValue::Map(right)) => {
            let Some(left) = json_object_entries(left) else {
                return false;
            };
            let Some(right) = json_object_entries(right) else {
                return false;
            };
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| json_cbor_semantically_equal(left, right))
                })
        }
        _ => left == right,
    }
}

fn json_object_entries(entries: &[(CborValue, CborValue)]) -> Option<BTreeMap<&str, &CborValue>> {
    let mut object = BTreeMap::new();
    for (key, value) in entries {
        let CborValue::Text(key) = key else {
            return None;
        };
        if object.insert(key.as_str(), value).is_some() {
            return None;
        }
    }
    Some(object)
}

impl Serialize for OpaqueProviderItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct Repr<'a> {
            tau_opaque_provider_item_version: u8,
            value: &'a CborValue,
            raw_json: &'a str,
        }

        Repr {
            // Keep at zero per `GATE-no-backward-compatibility`.
            tau_opaque_provider_item_version: 0,
            value: &self.value,
            raw_json: &self.raw_json,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OpaqueProviderItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Current {
            tau_opaque_provider_item_version: u8,
            value: CborValue,
            raw_json: String,
        }

        match Current::deserialize(deserializer)? {
            Current {
                tau_opaque_provider_item_version: 0,
                value,
                raw_json,
            } => Self::try_new(value, raw_json).map_err(de::Error::custom),
            Current {
                tau_opaque_provider_item_version,
                ..
            } => Err(de::Error::custom(format!(
                "unsupported opaque provider item version {tau_opaque_provider_item_version}"
            ))),
        }
    }
}

/// One message item in the prompt or assistant output timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageItem {
    /// Role that authored the message.
    pub role: ContextRole,
    /// Ordered content parts for the message.
    pub content: Vec<ContentPart>,
    /// Optional assistant-message phase metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<MessagePhase>,
    /// Optional raw Responses assistant message item for replay fidelity.
    ///
    /// This sidecar is provider-visible syntax only. Consumers that render,
    /// validate, or make semantic decisions must use [`Self::role`],
    /// [`Self::content`], and [`Self::phase`]. The ChatGPT/Codex Responses
    /// backend may use this to preserve provider item ids, statuses,
    /// annotations, content-part boundaries, and unknown fields while
    /// deliberately rebasing replayed text and phase from the typed fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses_raw_json: Option<String>,
}

/// One tool call item in the prompt or assistant output timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallItem {
    /// Stable tool-call identifier.
    pub call_id: ToolCallId,
    /// Tool name requested by the assistant.
    pub name: ToolName,
    /// Kind of tool call.
    pub tool_type: ToolType,
    /// Tool arguments in protocol CBOR form.
    pub arguments: CborValue,
    /// Provider-produced raw JSON text for function-call arguments.
    ///
    /// Providers usually expose function arguments as a JSON string. Tau parses
    /// that string into [`Self::arguments`] for validation and tool dispatch,
    /// but replay needs the original string so provider cache identity is
    /// not changed by JSON map reordering or normalization. Older persisted
    /// turns and non-function/custom tool calls leave this empty and
    /// providers fall back to serializing [`Self::arguments`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_arguments_json: Option<String>,
    /// Optional Responses provider item envelope for replay fidelity.
    ///
    /// Responses output items have a provider-owned item `id`, may carry a
    /// `status`, and can gain future envelope fields. Tau keeps the semantic
    /// tool-call fields above as authoritative for validation and dispatch, but
    /// Responses replay uses this sidecar to keep provider-visible transcript
    /// identity stable when rebuilding `function_call` and `custom_tool_call`
    /// input items. Old persisted turns and non-Responses providers leave this
    /// empty and providers fall back to deterministic synthesis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses_envelope: Option<ResponsesToolCallEnvelope>,
}

/// Provider-owned Responses envelope fields for one tool-call output item.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResponsesToolCallEnvelope {
    /// Provider output item id, distinct from the semantic tool-call `call_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// Provider output item status, if the Responses item carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Unknown provider envelope fields preserved for full-transcript replay.
    ///
    /// This must be a [`CborValue::Map`] corresponding to parsed JSON object
    /// members. It preserves field values, but not raw JSON spelling, duplicate
    /// keys, or object-member order. The map intentionally excludes structured
    /// Responses tool-call fields such as `type`, `id`, `status`, `call_id`,
    /// `name`, `arguments`, and `input`; those are rebuilt from
    /// [`ToolCallItem`] or from the named envelope fields above, so
    /// harness-side tool-call id normalization remains authoritative and
    /// extra fields cannot override semantic replay fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_fields: Option<CborValue>,
}

impl ResponsesToolCallEnvelope {
    /// Returns whether the envelope carries no provider replay fields.
    pub fn is_empty(&self) -> bool {
        self.item_id.is_none() && self.status.is_none() && self.extra_fields.is_none()
    }
}

/// Terminal status for one tool result item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    /// Tool completed successfully.
    Success,
    /// Tool failed with a diagnostic message.
    Error {
        /// Human-readable failure message.
        message: String,
    },
    /// Tool execution was cancelled.
    Cancelled {
        /// Human-readable cancellation reason.
        reason: String,
    },
}

/// Closed media type for image bytes carried in provider-visible tool output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageMediaType {
    /// Portable Network Graphics.
    Png,
    /// Joint Photographic Experts Group image.
    Jpeg,
    /// WebP image.
    Webp,
}

impl ImageMediaType {
    /// Return the canonical MIME type for this image format.
    #[must_use]
    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }
}

/// Provider image-detail mode selected when preparing an image.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageDetail {
    /// Bounded high-detail image input.
    High,
}

/// Validated image bytes carried as typed provider-visible tool content.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImageContent {
    /// Closed media type derived from the decoded image.
    pub media_type: ImageMediaType,
    /// Canonical encoded image bytes.
    #[serde(with = "arc_bytes")]
    pub data: Arc<[u8]>,
    /// Decoded image width in pixels.
    pub width: u32,
    /// Decoded image height in pixels.
    pub height: u32,
    /// Provider detail mode used when the bytes were prepared.
    pub detail: ImageDetail,
}

impl fmt::Debug for ImageContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageContent")
            .field("media_type", &self.media_type)
            .field("data", &format_args!("<{} bytes>", self.data.len()))
            .field("width", &self.width)
            .field("height", &self.height)
            .field("detail", &self.detail)
            .finish()
    }
}

/// One typed provider-visible content part attached to a successful tool
/// result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ToolResultContentPart {
    /// A validated local raster image.
    Image(ImageContent),
}

/// One rendered header in the text sent to a provider for a tool response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolResponseHeader {
    /// Header key rendered before the `: ` separator.
    pub key: String,
    /// Header value rendered after the `: ` separator.
    pub value: String,
}

/// Provider-facing text form of a tool response.
///
/// The canonical rendering is header lines in `<key>: <value>` form, followed
/// by an empty line and then the tool-specific body. [`Self::render`] applies a
/// final provider-visible safety pass: headers are forced to single lines,
/// controls and Unicode line/paragraph separators are escaped, and body ASCII
/// line feeds are preserved as record separators while other controls and
/// separators are escaped. Tool result events still carry
/// raw CBOR so extensions do not need to coordinate a wire-format migration;
/// this type is the normalized boundary used before provider output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResponse {
    /// Original tool payload kept for non-provider consumers that need
    /// structured data rather than rendered text.
    pub raw: CborValue,
    /// Structured headers rendered before the response body.
    pub headers: Vec<ToolResponseHeader>,
    /// Tool-specific response text rendered after the blank separator.
    pub body: String,
}

impl ToolResponse {
    /// Builds a normalized provider-facing response from a raw CBOR tool
    /// result.
    #[must_use]
    pub fn from_cbor(value: &CborValue) -> Self {
        match value {
            CborValue::Map(entries) => Self::from_cbor_map(entries),
            other => Self {
                raw: other.clone(),
                headers: Vec::new(),
                body: cbor_tool_response_text(other),
            },
        }
    }

    /// Renders this response as header lines, a blank line, then body text.
    ///
    /// This is the last provider-visible defense-in-depth boundary. It escapes
    /// header controls and Unicode line/paragraph separators, escapes body
    /// controls and separators except for legitimate ASCII `\n` record
    /// separators, and never emits raw ESC, CR, NUL, DEL, C1 controls, or
    /// Unicode line/paragraph separators.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write_rendered(&mut out)
            .expect("writing a tool response to String cannot fail");
        out
    }

    fn write_rendered(&self, out: &mut impl fmt::Write) -> fmt::Result {
        for header in &self.headers {
            write_sanitized_provider_text(&header.key, ProviderTextMode::Header, out)?;
            out.write_str(": ")?;
            write_sanitized_provider_text(&header.value, ProviderTextMode::Header, out)?;
            out.write_char('\n')?;
        }
        if !self.headers.is_empty() {
            out.write_char('\n')?;
        }
        write_sanitized_provider_text(&self.body, ProviderTextMode::Body, out)
    }

    fn from_cbor_map(entries: &[(CborValue, CborValue)]) -> Self {
        let raw = CborValue::Map(entries.to_vec());
        let mut projection = MaterializedMapProjection::default();
        visit_cbor_tool_response_map(entries, None, &mut projection)
            .expect("materialized map projection cannot fail");
        Self {
            raw,
            headers: projection.headers,
            body: projection.body,
        }
    }
}

fn sanitize_provider_header_text(input: &str) -> String {
    sanitize_provider_text(input, ProviderTextMode::Header)
}

#[derive(Clone, Copy)]
enum ProviderTextMode {
    Header,
    Body,
}

fn sanitize_provider_text(input: &str, mode: ProviderTextMode) -> String {
    let mut output = String::new();
    write_sanitized_provider_text(input, mode, &mut output)
        .expect("writing sanitized provider text to String cannot fail");
    output
}

fn write_sanitized_provider_text(
    input: &str,
    mode: ProviderTextMode,
    output: &mut dyn fmt::Write,
) -> fmt::Result {
    for ch in input.chars() {
        match ch {
            '\n' if matches!(mode, ProviderTextMode::Body) => output.write_char('\n')?,
            '\n' => output.write_str("\\n")?,
            '\r' => output.write_str("\\r")?,
            '\t' => output.write_str("\\t")?,
            '\0' => output.write_str("\\0")?,
            '\u{1b}' => output.write_str("\\x1b")?,
            '\u{2028}' => output.write_str("\\u{2028}")?,
            '\u{2029}' => output.write_str("\\u{2029}")?,
            ch if is_provider_unsafe_control(ch) => {
                write!(output, "\\u{{{:x}}}", ch as u32)?;
            }
            ch => output.write_char(ch)?,
        }
    }
    Ok(())
}

fn is_provider_unsafe_control(ch: char) -> bool {
    matches!(ch, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}')
}

fn cbor_tool_response_text(value: &CborValue) -> String {
    let mut output = String::new();
    write_provider_tool_result_text(value, ProviderToolResultStatus::Success, &mut output)
        .expect("writing CBOR tool response text to String cannot fail");
    output
}

/// Borrowed terminal status for rendering or measuring canonical
/// provider-facing text directly from raw CBOR.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderToolResultStatus<'a> {
    /// The tool returned successfully.
    Success,
    /// The tool failed with the supplied message.
    Error {
        /// Producer-supplied error message.
        message: &'a str,
    },
    /// The tool was cancelled with the supplied reason.
    Cancelled {
        /// Producer-supplied cancellation reason.
        reason: &'a str,
    },
}

/// Exact size of canonical provider-facing tool-result text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderToolResultTextMeasurement {
    /// UTF-8 bytes in the complete rendered text before outer-envelope
    /// escaping.
    pub rendered_bytes: usize,
}

/// Measure canonical provider-facing tool text in linear time without
/// materializing it or retaining source-sized rendering state.
#[must_use]
pub fn measure_provider_tool_result_text(
    value: &CborValue,
    status: ProviderToolResultStatus<'_>,
) -> ProviderToolResultTextMeasurement {
    let mut cache = None;
    ProviderToolResultTextMeasurement {
        rendered_bytes: provider_tool_result_text_shape(value, status, &mut cache).raw_bytes,
    }
}

/// Stream the same canonical provider-facing text as
/// [`ToolResultItem::render_provider_text`] directly from raw CBOR.
///
/// Classification is cached once per rendered CBOR node, so nested maps remain
/// linear rather than recursively rescanning their descendants. Callers that
/// need a hard memory bound should measure first and invoke this renderer only
/// when the measured output fits that bound.
pub fn write_provider_tool_result_text(
    value: &CborValue,
    status: ProviderToolResultStatus<'_>,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    let mut cache = Some(HashMap::new());
    let _ = provider_tool_result_text_cached_shape(value, status, &mut cache);
    write_provider_tool_result_text_with_cache(
        value,
        status,
        cache.as_ref().expect("provider text cache enabled"),
        output,
    )
}

/// Stream the canonical single-line provider rendering of one tool-result
/// header value.
///
/// Callers that need only a bounded prefix can provide a bounded
/// [`fmt::Write`] sink without allocating the complete sanitized value.
pub fn write_provider_tool_header_text(input: &str, output: &mut impl fmt::Write) -> fmt::Result {
    write_sanitized_provider_text(input, ProviderTextMode::Header, output)
}

#[derive(Clone, Copy, Debug, Default)]
struct ProviderTextShape {
    /// Bytes in the raw canonical rendering.
    raw_bytes: usize,
    /// Bytes after applying header sanitization.
    header_bytes: usize,
    /// Bytes after applying body sanitization.
    body_bytes: usize,
    /// Literal newlines in the raw canonical rendering.
    newlines: usize,
    /// Literal newlines at the end of the raw canonical rendering.
    trailing_newlines: usize,
    /// Whether the raw canonical rendering contains any non-newline byte.
    has_non_newline: bool,
    /// Bounded exact-key signature of the raw canonical rendering.
    raw_key: ProviderKeySignature,
    /// Bounded exact-key signature after header sanitization.
    header_key: ProviderKeySignature,
    /// Bounded exact-key signature after body sanitization.
    body_key: ProviderKeySignature,
}

#[cfg(test)]
thread_local! {
    static PROVIDER_TEXT_SHAPE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PROVIDER_TEXT_SHAPE_CACHE_INSERTIONS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_provider_text_shape_visits() {
    PROVIDER_TEXT_SHAPE_VISITS.with(|visits| visits.set(0));
    PROVIDER_TEXT_SHAPE_CACHE_INSERTIONS.with(|insertions| insertions.set(0));
}

#[cfg(test)]
fn provider_text_shape_visits() -> usize {
    PROVIDER_TEXT_SHAPE_VISITS.with(Cell::get)
}

#[cfg(test)]
fn provider_text_shape_cache_insertions() -> usize {
    PROVIDER_TEXT_SHAPE_CACHE_INSERTIONS.with(Cell::get)
}

impl ProviderTextShape {
    fn literal(text: &str) -> Self {
        let mut shape = Self::default();
        shape
            .raw_key
            .write_str(text)
            .expect("bounded key signature cannot fail");
        write_sanitized_provider_text(text, ProviderTextMode::Header, &mut shape.header_key)
            .expect("bounded key signature cannot fail");
        write_sanitized_provider_text(text, ProviderTextMode::Body, &mut shape.body_key)
            .expect("bounded key signature cannot fail");
        for character in text.chars() {
            let bytes = character.len_utf8();
            shape.raw_bytes = shape.raw_bytes.saturating_add(bytes);
            shape.header_bytes =
                shape
                    .header_bytes
                    .saturating_add(sanitized_provider_character_bytes(
                        character,
                        ProviderTextMode::Header,
                    ));
            shape.body_bytes = shape
                .body_bytes
                .saturating_add(sanitized_provider_character_bytes(
                    character,
                    ProviderTextMode::Body,
                ));
            if character == '\n' {
                shape.newlines = shape.newlines.saturating_add(1);
                shape.trailing_newlines = shape.trailing_newlines.saturating_add(1);
            } else {
                shape.trailing_newlines = 0;
                shape.has_non_newline = true;
            }
        }
        shape
    }

    fn append(&mut self, suffix: Self) {
        self.raw_bytes = self.raw_bytes.saturating_add(suffix.raw_bytes);
        self.header_bytes = self.header_bytes.saturating_add(suffix.header_bytes);
        self.body_bytes = self.body_bytes.saturating_add(suffix.body_bytes);
        self.newlines = self.newlines.saturating_add(suffix.newlines);
        self.trailing_newlines = if suffix.has_non_newline {
            suffix.trailing_newlines
        } else {
            self.trailing_newlines
                .saturating_add(suffix.trailing_newlines)
        };
        self.has_non_newline |= suffix.has_non_newline;
        self.raw_key.append(suffix.raw_key);
        self.header_key.append(suffix.header_key);
        self.body_key.append(suffix.body_key);
    }

    fn sanitized(self, mode: ProviderTextMode) -> Self {
        let (raw_bytes, raw_key) = match mode {
            ProviderTextMode::Header => (self.header_bytes, self.header_key),
            ProviderTextMode::Body => (self.body_bytes, self.body_key),
        };
        if matches!(mode, ProviderTextMode::Header) {
            return Self {
                raw_bytes,
                header_bytes: raw_bytes,
                body_bytes: raw_bytes,
                newlines: 0,
                trailing_newlines: 0,
                has_non_newline: raw_bytes != 0,
                raw_key,
                header_key: raw_key,
                body_key: raw_key,
            };
        }
        Self {
            raw_bytes,
            header_bytes: raw_bytes.saturating_add(self.newlines),
            body_bytes: raw_bytes,
            newlines: self.newlines,
            trailing_newlines: self.trailing_newlines,
            has_non_newline: self.has_non_newline,
            raw_key,
            header_key: self
                .body_key
                .sanitized_newlines(ProviderTextMode::Header, self.newlines),
            body_key: raw_key,
        }
    }

    fn trim_trailing_newlines(mut self) -> Self {
        self.raw_bytes = self.raw_bytes.saturating_sub(self.trailing_newlines);
        self.header_bytes = self
            .header_bytes
            .saturating_sub(self.trailing_newlines.saturating_mul(2));
        self.body_bytes = self.body_bytes.saturating_sub(self.trailing_newlines);
        self.newlines = self.newlines.saturating_sub(self.trailing_newlines);
        self.raw_key.trim_suffix(self.trailing_newlines);
        self.header_key
            .trim_suffix(self.trailing_newlines.saturating_mul(2));
        self.body_key.trim_suffix(self.trailing_newlines);
        self.trailing_newlines = 0;
        self
    }

    fn has_newline(self) -> bool {
        self.newlines != 0
    }
}

const PROVIDER_KEY_SIGNATURE_BYTES: usize = "line-numbered content".len();

/// Bounded prefix plus exact length for recognizing the only rendered map keys
/// with provider projection semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderKeySignature {
    /// Retained prefix bytes, capped at the longest semantic key.
    bytes: [u8; PROVIDER_KEY_SIGNATURE_BYTES],
    /// Complete canonical rendered byte length, including discarded suffix.
    total_bytes: usize,
}

impl Default for ProviderKeySignature {
    fn default() -> Self {
        Self {
            bytes: [0; PROVIDER_KEY_SIGNATURE_BYTES],
            total_bytes: 0,
        }
    }
}

impl ProviderKeySignature {
    fn append(&mut self, suffix: Self) {
        let retained = self.total_bytes.min(PROVIDER_KEY_SIGNATURE_BYTES);
        let available = PROVIDER_KEY_SIGNATURE_BYTES.saturating_sub(retained);
        let copied = suffix.total_bytes.min(available);
        self.bytes[retained..retained + copied].copy_from_slice(&suffix.bytes[..copied]);
        self.total_bytes = self.total_bytes.saturating_add(suffix.total_bytes);
    }

    fn trim_suffix(&mut self, bytes: usize) {
        self.total_bytes = self.total_bytes.saturating_sub(bytes);
    }

    fn kind(self) -> CborMapKeyKind {
        let bytes = &self.bytes[..self.total_bytes.min(PROVIDER_KEY_SIGNATURE_BYTES)];
        match bytes {
            b"data" if self.total_bytes == 4 => CborMapKeyKind::Data,
            b"output" if self.total_bytes == 6 => CborMapKeyKind::Output,
            b"line-numbered content" if self.total_bytes == PROVIDER_KEY_SIGNATURE_BYTES => {
                CborMapKeyKind::LineNumberedContent
            }
            _ => CborMapKeyKind::Other,
        }
    }

    fn sanitized_newlines(self, mode: ProviderTextMode, newlines: usize) -> Self {
        if newlines == 0 || matches!(mode, ProviderTextMode::Body) {
            return self;
        }
        // Any header-sanitized newline introduces `\`, which cannot occur in a
        // recognized semantic key.
        Self {
            bytes: self.bytes,
            total_bytes: self.total_bytes.saturating_add(newlines),
        }
    }
}

impl fmt::Write for ProviderKeySignature {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let retained = self.total_bytes.min(PROVIDER_KEY_SIGNATURE_BYTES);
        let available = PROVIDER_KEY_SIGNATURE_BYTES.saturating_sub(retained);
        let copied = text.len().min(available);
        self.bytes[retained..retained + copied].copy_from_slice(&text.as_bytes()[..copied]);
        self.total_bytes = self.total_bytes.saturating_add(text.len());
        Ok(())
    }
}

fn sanitized_provider_character_bytes(character: char, mode: ProviderTextMode) -> usize {
    match character {
        '\n' if matches!(mode, ProviderTextMode::Body) => 1,
        '\n' | '\r' | '\t' | '\0' => 2,
        '\u{1b}' => 4,
        '\u{2028}' | '\u{2029}' => 8,
        character if is_provider_unsafe_control(character) => {
            4 + format!("{:x}", character as u32).len()
        }
        character => character.len_utf8(),
    }
}

fn provider_tool_result_text_shape(
    value: &CborValue,
    status: ProviderToolResultStatus<'_>,
    cache: &mut Option<HashMap<usize, ProviderTextShape>>,
) -> ProviderTextShape {
    match status {
        ProviderToolResultStatus::Success => cbor_tool_response_shape(value, None, cache),
        ProviderToolResultStatus::Error { message } => {
            cbor_tool_response_shape(value, Some(("error", message)), cache)
        }
        ProviderToolResultStatus::Cancelled { reason } => {
            let mut shape =
                ProviderTextShape::literal("cancelled").sanitized(ProviderTextMode::Header);
            shape.append(ProviderTextShape::literal(": "));
            shape.append(ProviderTextShape::literal(reason).sanitized(ProviderTextMode::Header));
            shape.append(ProviderTextShape::literal("\n\n"));
            shape
        }
    }
}

fn provider_tool_result_text_cached_shape(
    value: &CborValue,
    status: ProviderToolResultStatus<'_>,
    cache: &mut Option<HashMap<usize, ProviderTextShape>>,
) -> ProviderTextShape {
    match status {
        ProviderToolResultStatus::Success => cbor_tool_response_cached_shape(value, None, cache),
        ProviderToolResultStatus::Error { message } => {
            cbor_tool_response_cached_shape(value, Some(("error", message)), cache)
        }
        ProviderToolResultStatus::Cancelled { reason } => {
            let mut shape =
                ProviderTextShape::literal("cancelled").sanitized(ProviderTextMode::Header);
            shape.append(ProviderTextShape::literal(": "));
            shape.append(ProviderTextShape::literal(reason).sanitized(ProviderTextMode::Header));
            shape.append(ProviderTextShape::literal("\n\n"));
            shape
        }
    }
}

fn cbor_tool_response_shape(
    value: &CborValue,
    prefix_header: Option<(&str, &str)>,
    cache: &mut Option<HashMap<usize, ProviderTextShape>>,
) -> ProviderTextShape {
    let CborValue::Map(entries) = value else {
        let mut shape = ProviderTextShape::default();
        if let Some((key, value)) = prefix_header {
            shape.append(ProviderTextShape::literal(key).sanitized(ProviderTextMode::Header));
            shape.append(ProviderTextShape::literal(": "));
            shape.append(ProviderTextShape::literal(value).sanitized(ProviderTextMode::Header));
            shape.append(ProviderTextShape::literal("\n\n"));
        }
        shape.append(cbor_tool_response_text_shape(value, cache).sanitized(ProviderTextMode::Body));
        return shape;
    };
    cbor_tool_response_map_shape(entries, prefix_header, cache)
}

fn cbor_tool_response_cached_shape(
    value: &CborValue,
    prefix_header: Option<(&str, &str)>,
    cache: &mut Option<HashMap<usize, ProviderTextShape>>,
) -> ProviderTextShape {
    let CborValue::Map(entries) = value else {
        let mut shape = ProviderTextShape::default();
        if let Some((key, value)) = prefix_header {
            shape.append(ProviderTextShape::literal(key).sanitized(ProviderTextMode::Header));
            shape.append(ProviderTextShape::literal(": "));
            shape.append(ProviderTextShape::literal(value).sanitized(ProviderTextMode::Header));
            shape.append(ProviderTextShape::literal("\n\n"));
        }
        shape.append(
            cbor_tool_response_text_cached_shape(value, cache).sanitized(ProviderTextMode::Body),
        );
        return shape;
    };
    cbor_tool_response_map_cached_shape(entries, prefix_header, cache)
}

fn cbor_tool_response_map_cached_shape(
    entries: &[(CborValue, CborValue)],
    prefix_header: Option<(&str, &str)>,
    cache: &mut Option<HashMap<usize, ProviderTextShape>>,
) -> ProviderTextShape {
    let mut projection = ShapeMapProjection {
        cache,
        rendered: ProviderTextShape::default(),
    };
    visit_cbor_tool_response_map(entries, prefix_header, &mut projection)
        .expect("shape projection cannot fail");
    projection.rendered
}

fn cbor_tool_response_map_shape(
    entries: &[(CborValue, CborValue)],
    prefix_header: Option<(&str, &str)>,
    cache: &mut Option<HashMap<usize, ProviderTextShape>>,
) -> ProviderTextShape {
    let mut without_output = MapShapeAccumulator::new(prefix_header);
    let mut with_output = MapShapeAccumulator::new(prefix_header);
    let mut has_output = false;
    for (key, value) in entries {
        let key_shape = cbor_tool_response_text_shape(key, cache);
        let value_shape = cbor_tool_response_text_shape(value, cache);
        let key_kind = key_shape.raw_key.kind();
        has_output |= matches!(
            key_kind,
            CborMapKeyKind::Output | CborMapKeyKind::LineNumberedContent
        );
        without_output.push(
            classify_map_entry(false, key_kind, value_shape.has_newline()),
            key_shape,
            value_shape,
        );
        with_output.push(
            classify_map_entry(true, key_kind, value_shape.has_newline()),
            key_shape,
            value_shape,
        );
    }
    if has_output {
        with_output.finish()
    } else {
        without_output.finish()
    }
}

/// Constant-state shape accumulator for one candidate map projection.
struct MapShapeAccumulator {
    /// Canonical header-region shape.
    headers: ProviderTextShape,
    /// Canonical body-region shape.
    body: ProviderTextShape,
    /// Whether at least one header was selected.
    wrote_header: bool,
    /// Whether at least one body entry was selected.
    wrote_body: bool,
}

impl MapShapeAccumulator {
    fn new(prefix_header: Option<(&str, &str)>) -> Self {
        let mut accumulator = Self {
            headers: ProviderTextShape::default(),
            body: ProviderTextShape::default(),
            wrote_header: false,
            wrote_body: false,
        };
        if let Some((key, value)) = prefix_header {
            accumulator
                .headers
                .append(ProviderTextShape::literal(key).sanitized(ProviderTextMode::Header));
            accumulator.headers.append(ProviderTextShape::literal(": "));
            accumulator
                .headers
                .append(ProviderTextShape::literal(value).sanitized(ProviderTextMode::Header));
            accumulator.headers.append(ProviderTextShape::literal("\n"));
            accumulator.wrote_header = true;
        }
        accumulator
    }

    fn push(
        &mut self,
        projection: MapEntryProjection,
        key: ProviderTextShape,
        value: ProviderTextShape,
    ) {
        match projection {
            MapEntryProjection::Suppressed => {}
            MapEntryProjection::Header => {
                self.headers.append(key.sanitized(ProviderTextMode::Header));
                self.headers.append(ProviderTextShape::literal(": "));
                self.headers
                    .append(value.sanitized(ProviderTextMode::Header));
                self.headers.append(ProviderTextShape::literal("\n"));
                self.wrote_header = true;
            }
            MapEntryProjection::Body { label } => {
                if self.wrote_body {
                    self.body.append(ProviderTextShape::literal("\n"));
                }
                if label {
                    self.body.append(key.sanitized(ProviderTextMode::Header));
                    self.body.append(ProviderTextShape::literal(":\n"));
                }
                self.body.append(value.sanitized(ProviderTextMode::Body));
                self.wrote_body = true;
            }
        }
    }

    fn finish(mut self) -> ProviderTextShape {
        if self.wrote_header {
            self.headers.append(ProviderTextShape::literal("\n"));
        }
        self.headers.append(self.body);
        self.headers
    }
}

fn cbor_tool_response_text_shape(
    value: &CborValue,
    cache: &mut Option<HashMap<usize, ProviderTextShape>>,
) -> ProviderTextShape {
    let key = value as *const CborValue as usize;
    if let Some(shape) = cache.as_ref().and_then(|cache| cache.get(&key)).copied() {
        return shape;
    }
    #[cfg(test)]
    PROVIDER_TEXT_SHAPE_VISITS.with(|visits| visits.set(visits.get().saturating_add(1)));
    let shape = match value {
        CborValue::Null => ProviderTextShape::default(),
        CborValue::Bool(value) => ProviderTextShape::literal(if *value { "true" } else { "false" }),
        CborValue::Integer(value) => {
            let value: i128 = (*value).into();
            ProviderTextShape::literal(&value.to_string())
        }
        CborValue::Float(value) => ProviderTextShape::literal(&value.to_string()),
        CborValue::Text(value) => ProviderTextShape::literal(value),
        CborValue::Bytes(value) => ProviderTextShape::literal(&format!("<{} bytes>", value.len())),
        CborValue::Array(values) => {
            let separator = if values
                .iter()
                .any(|value| matches!(value, CborValue::Map(_)))
            {
                "\n\n"
            } else {
                "\n"
            };
            let mut shape = ProviderTextShape::default();
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    shape.append(ProviderTextShape::literal(separator));
                }
                let child = cbor_tool_response_text_shape(value, cache);
                shape.append(if matches!(value, CborValue::Map(_)) {
                    child.trim_trailing_newlines()
                } else {
                    child
                });
            }
            shape
        }
        CborValue::Map(entries) => cbor_tool_response_map_shape(entries, None, cache),
        CborValue::Tag(_, inner) => cbor_tool_response_text_shape(inner, cache),
        _ => ProviderTextShape::default(),
    };
    if let Some(cache) = cache.as_mut() {
        #[cfg(test)]
        PROVIDER_TEXT_SHAPE_CACHE_INSERTIONS
            .with(|insertions| insertions.set(insertions.get().saturating_add(1)));
        cache.insert(key, shape);
    }
    shape
}

fn cbor_tool_response_text_cached_shape(
    value: &CborValue,
    cache: &mut Option<HashMap<usize, ProviderTextShape>>,
) -> ProviderTextShape {
    let key = value as *const CborValue as usize;
    if let Some(shape) = cache.as_ref().and_then(|cache| cache.get(&key)).copied() {
        return shape;
    }
    let shape = match value {
        CborValue::Null => ProviderTextShape::default(),
        CborValue::Bool(value) => ProviderTextShape::literal(if *value { "true" } else { "false" }),
        CborValue::Integer(value) => {
            let value: i128 = (*value).into();
            ProviderTextShape::literal(&value.to_string())
        }
        CborValue::Float(value) => ProviderTextShape::literal(&value.to_string()),
        CborValue::Text(value) => ProviderTextShape::literal(value),
        CborValue::Bytes(value) => ProviderTextShape::literal(&format!("<{} bytes>", value.len())),
        CborValue::Array(values) => {
            let separator = if values
                .iter()
                .any(|value| matches!(value, CborValue::Map(_)))
            {
                "\n\n"
            } else {
                "\n"
            };
            let mut shape = ProviderTextShape::default();
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    shape.append(ProviderTextShape::literal(separator));
                }
                let child = cbor_tool_response_text_cached_shape(value, cache);
                shape.append(if matches!(value, CborValue::Map(_)) {
                    child.trim_trailing_newlines()
                } else {
                    child
                });
            }
            shape
        }
        CborValue::Map(entries) => cbor_tool_response_map_cached_shape(entries, None, cache),
        CborValue::Tag(_, inner) => cbor_tool_response_text_cached_shape(inner, cache),
        _ => ProviderTextShape::default(),
    };
    if let Some(cache) = cache.as_mut() {
        cache.insert(key, shape);
    }
    shape
}

/// Semantic classification of a canonically rendered CBOR map key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CborMapKeyKind {
    /// Exact rendered key `data`.
    Data,
    /// Exact rendered key `output`.
    Output,
    /// Exact rendered key `line-numbered content`.
    LineNumberedContent,
    /// Every other rendered key.
    Other,
}

/// Provider projection selected for one canonical map entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MapEntryProjection {
    /// Omit redundant raw data when a rendered output field exists.
    Suppressed,
    /// Render as one sanitized single-line header.
    Header,
    /// Render as body text, optionally preceded by a sanitized key label.
    Body {
        /// Whether to emit `<key>:\n` before the value.
        label: bool,
    },
}

/// Consumer operations for the shared map projection traversal.
trait CborMapProjectionOps {
    /// Classify one canonically rendered key.
    fn key_kind(&mut self, key: &CborValue) -> CborMapKeyKind;
    /// Report whether one canonically rendered value contains a newline.
    fn value_has_newline(&mut self, value: &CborValue) -> bool;
    /// Emit the optional status prefix as the first header.
    fn prefix_header(&mut self, key: &str, value: &str) -> fmt::Result;
    /// Emit one ordinary header entry.
    fn header(&mut self, key: &CborValue, value: &CborValue) -> fmt::Result;
    /// Emit the separator between non-empty header and body regions.
    fn header_body_separator(&mut self) -> fmt::Result;
    /// Emit one body entry and its canonical inter-entry separator.
    fn body(
        &mut self,
        key: &CborValue,
        value: &CborValue,
        label: bool,
        separator_before: bool,
    ) -> fmt::Result;
}

/// Traverse one map in canonical provider order: all headers, one region
/// separator, then all body entries with their exact inter-entry separators.
fn visit_cbor_tool_response_map(
    entries: &[(CborValue, CborValue)],
    prefix_header: Option<(&str, &str)>,
    ops: &mut impl CborMapProjectionOps,
) -> fmt::Result {
    let has_output = entries.iter().any(|(key, _)| {
        matches!(
            ops.key_kind(key),
            CborMapKeyKind::Output | CborMapKeyKind::LineNumberedContent
        )
    });
    let mut wrote_header = false;
    if let Some((key, value)) = prefix_header {
        ops.prefix_header(key, value)?;
        wrote_header = true;
    }
    for (key, value) in entries {
        let key_kind = ops.key_kind(key);
        if classify_map_entry(has_output, key_kind, false) == MapEntryProjection::Suppressed {
            continue;
        }
        if classify_map_entry(has_output, key_kind, ops.value_has_newline(value))
            == MapEntryProjection::Header
        {
            ops.header(key, value)?;
            wrote_header = true;
        }
    }
    if wrote_header {
        ops.header_body_separator()?;
    }
    let mut wrote_body = false;
    for (key, value) in entries {
        let key_kind = ops.key_kind(key);
        if classify_map_entry(has_output, key_kind, false) == MapEntryProjection::Suppressed {
            continue;
        }
        if let MapEntryProjection::Body { label } =
            classify_map_entry(has_output, key_kind, ops.value_has_newline(value))
        {
            ops.body(key, value, label, wrote_body)?;
            wrote_body = true;
        }
    }
    Ok(())
}

fn cbor_map_key_kind_from_text(text: &str) -> CborMapKeyKind {
    match text {
        "data" => CborMapKeyKind::Data,
        "output" => CborMapKeyKind::Output,
        "line-numbered content" => CborMapKeyKind::LineNumberedContent,
        _ => CborMapKeyKind::Other,
    }
}

fn classify_map_entry(
    has_output: bool,
    key: CborMapKeyKind,
    value_has_newline: bool,
) -> MapEntryProjection {
    if has_output && key == CborMapKeyKind::Data {
        MapEntryProjection::Suppressed
    } else if matches!(
        key,
        CborMapKeyKind::Output | CborMapKeyKind::LineNumberedContent
    ) {
        MapEntryProjection::Body { label: false }
    } else if value_has_newline {
        MapEntryProjection::Body { label: true }
    } else {
        MapEntryProjection::Header
    }
}

/// Materialized [`ToolResponse`] consumer for the shared map traversal.
#[derive(Default)]
struct MaterializedMapProjection {
    /// Canonical header entries.
    headers: Vec<ToolResponseHeader>,
    /// Canonical body text.
    body: String,
    /// Per-node canonical text cache used by classification and emission.
    rendered_text: HashMap<usize, String>,
}

impl MaterializedMapProjection {
    fn rendered(&mut self, value: &CborValue) -> String {
        let key = value as *const CborValue as usize;
        if let Some(rendered) = self.rendered_text.get(&key) {
            return rendered.clone();
        }
        let rendered = cbor_tool_response_text(value);
        self.rendered_text.insert(key, rendered.clone());
        rendered
    }
}

impl CborMapProjectionOps for MaterializedMapProjection {
    fn key_kind(&mut self, key: &CborValue) -> CborMapKeyKind {
        cbor_map_key_kind_from_text(&cbor_tool_response_text(key))
    }

    fn value_has_newline(&mut self, value: &CborValue) -> bool {
        self.rendered(value).contains('\n')
    }

    fn prefix_header(&mut self, key: &str, value: &str) -> fmt::Result {
        self.headers.push(ToolResponseHeader {
            key: key.to_owned(),
            value: value.to_owned(),
        });
        Ok(())
    }

    fn header(&mut self, key: &CborValue, value: &CborValue) -> fmt::Result {
        let key = self.rendered(key);
        let value = self.rendered(value);
        self.headers.push(ToolResponseHeader { key, value });
        Ok(())
    }

    fn header_body_separator(&mut self) -> fmt::Result {
        Ok(())
    }

    fn body(
        &mut self,
        key: &CborValue,
        value: &CborValue,
        label: bool,
        separator_before: bool,
    ) -> fmt::Result {
        if separator_before {
            self.body.push('\n');
        }
        if label {
            let key = sanitize_provider_header_text(&self.rendered(key));
            self.body.push_str(&key);
            self.body.push_str(":\n");
        }
        let value = self.rendered(value);
        self.body.push_str(&value);
        Ok(())
    }
}

/// Byte-shape consumer for the shared map traversal.
struct ShapeMapProjection<'a> {
    /// Optional per-node shape cache.
    cache: &'a mut Option<HashMap<usize, ProviderTextShape>>,
    /// Accumulated exact rendered shape.
    rendered: ProviderTextShape,
}

impl CborMapProjectionOps for ShapeMapProjection<'_> {
    fn key_kind(&mut self, key: &CborValue) -> CborMapKeyKind {
        let mut no_cache = None;
        cbor_tool_response_text_shape(key, &mut no_cache)
            .raw_key
            .kind()
    }

    fn value_has_newline(&mut self, value: &CborValue) -> bool {
        cbor_tool_response_text_cached_shape(value, self.cache).has_newline()
    }

    fn prefix_header(&mut self, key: &str, value: &str) -> fmt::Result {
        self.rendered
            .append(ProviderTextShape::literal(key).sanitized(ProviderTextMode::Header));
        self.rendered.append(ProviderTextShape::literal(": "));
        self.rendered
            .append(ProviderTextShape::literal(value).sanitized(ProviderTextMode::Header));
        self.rendered.append(ProviderTextShape::literal("\n"));
        Ok(())
    }

    fn header(&mut self, key: &CborValue, value: &CborValue) -> fmt::Result {
        self.rendered.append(
            cbor_tool_response_text_cached_shape(key, self.cache)
                .sanitized(ProviderTextMode::Header),
        );
        self.rendered.append(ProviderTextShape::literal(": "));
        self.rendered.append(
            cbor_tool_response_text_cached_shape(value, self.cache)
                .sanitized(ProviderTextMode::Header),
        );
        self.rendered.append(ProviderTextShape::literal("\n"));
        Ok(())
    }

    fn header_body_separator(&mut self) -> fmt::Result {
        self.rendered.append(ProviderTextShape::literal("\n"));
        Ok(())
    }

    fn body(
        &mut self,
        key: &CborValue,
        value: &CborValue,
        label: bool,
        separator_before: bool,
    ) -> fmt::Result {
        if separator_before {
            self.rendered.append(ProviderTextShape::literal("\n"));
        }
        if label {
            self.rendered.append(
                cbor_tool_response_text_cached_shape(key, self.cache)
                    .sanitized(ProviderTextMode::Header),
            );
            self.rendered.append(ProviderTextShape::literal(":\n"));
        }
        self.rendered.append(
            cbor_tool_response_text_cached_shape(value, self.cache)
                .sanitized(ProviderTextMode::Body),
        );
        Ok(())
    }
}

fn cached_provider_text_shape(
    value: &CborValue,
    cache: &HashMap<usize, ProviderTextShape>,
) -> ProviderTextShape {
    cache
        .get(&(value as *const CborValue as usize))
        .copied()
        .expect("rendered CBOR node must have one cached shape")
}

fn write_provider_tool_result_text_with_cache(
    value: &CborValue,
    status: ProviderToolResultStatus<'_>,
    cache: &HashMap<usize, ProviderTextShape>,
    output: &mut dyn fmt::Write,
) -> fmt::Result {
    match status {
        ProviderToolResultStatus::Success => write_cbor_tool_response(value, None, cache, output),
        ProviderToolResultStatus::Error { message } => {
            write_cbor_tool_response(value, Some(("error", message)), cache, output)
        }
        ProviderToolResultStatus::Cancelled { reason } => {
            write_sanitized_provider_text("cancelled", ProviderTextMode::Header, output)?;
            output.write_str(": ")?;
            write_sanitized_provider_text(reason, ProviderTextMode::Header, output)?;
            output.write_str("\n\n")
        }
    }
}

fn write_cbor_tool_response(
    value: &CborValue,
    prefix_header: Option<(&str, &str)>,
    cache: &HashMap<usize, ProviderTextShape>,
    output: &mut dyn fmt::Write,
) -> fmt::Result {
    let CborValue::Map(entries) = value else {
        if let Some((key, value)) = prefix_header {
            write_sanitized_provider_text(key, ProviderTextMode::Header, output)?;
            output.write_str(": ")?;
            write_sanitized_provider_text(value, ProviderTextMode::Header, output)?;
            output.write_str("\n\n")?;
        }
        return write_sanitized_cbor_tool_response_text(
            value,
            ProviderTextMode::Body,
            cache,
            output,
        );
    };
    write_cbor_tool_response_map(entries, prefix_header, cache, output)
}

fn write_cbor_tool_response_map(
    entries: &[(CborValue, CborValue)],
    prefix_header: Option<(&str, &str)>,
    cache: &HashMap<usize, ProviderTextShape>,
    output: &mut dyn fmt::Write,
) -> fmt::Result {
    let mut projection = StreamingMapProjection { cache, output };
    visit_cbor_tool_response_map(entries, prefix_header, &mut projection)
}

/// Streaming text consumer for the shared map traversal.
struct StreamingMapProjection<'a> {
    /// Complete per-node shape cache built before streaming.
    cache: &'a HashMap<usize, ProviderTextShape>,
    /// Downstream canonical text sink.
    output: &'a mut dyn fmt::Write,
}

impl CborMapProjectionOps for StreamingMapProjection<'_> {
    fn key_kind(&mut self, key: &CborValue) -> CborMapKeyKind {
        let mut no_cache = None;
        cbor_tool_response_text_shape(key, &mut no_cache)
            .raw_key
            .kind()
    }

    fn value_has_newline(&mut self, value: &CborValue) -> bool {
        cached_provider_text_shape(value, self.cache).has_newline()
    }

    fn prefix_header(&mut self, key: &str, value: &str) -> fmt::Result {
        write_sanitized_provider_text(key, ProviderTextMode::Header, self.output)?;
        self.output.write_str(": ")?;
        write_sanitized_provider_text(value, ProviderTextMode::Header, self.output)?;
        self.output.write_char('\n')
    }

    fn header(&mut self, key: &CborValue, value: &CborValue) -> fmt::Result {
        write_sanitized_cbor_tool_response_text(
            key,
            ProviderTextMode::Header,
            self.cache,
            self.output,
        )?;
        self.output.write_str(": ")?;
        write_sanitized_cbor_tool_response_text(
            value,
            ProviderTextMode::Header,
            self.cache,
            self.output,
        )?;
        self.output.write_char('\n')
    }

    fn header_body_separator(&mut self) -> fmt::Result {
        self.output.write_char('\n')
    }

    fn body(
        &mut self,
        key: &CborValue,
        value: &CborValue,
        label: bool,
        separator_before: bool,
    ) -> fmt::Result {
        if separator_before {
            self.output.write_char('\n')?;
        }
        if label {
            write_sanitized_cbor_tool_response_text(
                key,
                ProviderTextMode::Header,
                self.cache,
                self.output,
            )?;
            self.output.write_str(":\n")?;
        }
        write_sanitized_cbor_tool_response_text(
            value,
            ProviderTextMode::Body,
            self.cache,
            self.output,
        )
    }
}

fn write_sanitized_cbor_tool_response_text(
    value: &CborValue,
    mode: ProviderTextMode,
    cache: &HashMap<usize, ProviderTextShape>,
    output: &mut dyn fmt::Write,
) -> fmt::Result {
    let mut sanitizer = ProviderTextSanitizer { mode, output };
    write_cbor_tool_response_text(value, cache, &mut sanitizer)
}

/// Adapter that sanitizes each rendered CBOR chunk for one provider text mode.
struct ProviderTextSanitizer<'a> {
    /// Header or body escaping rules applied to every chunk.
    mode: ProviderTextMode,
    /// Downstream canonical text sink.
    output: &'a mut dyn fmt::Write,
}

impl fmt::Write for ProviderTextSanitizer<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        write_sanitized_provider_text(text, self.mode, self.output)
    }
}

fn write_cbor_tool_response_text(
    value: &CborValue,
    cache: &HashMap<usize, ProviderTextShape>,
    output: &mut dyn fmt::Write,
) -> fmt::Result {
    match value {
        CborValue::Null => Ok(()),
        CborValue::Bool(value) => write!(output, "{value}"),
        CborValue::Integer(value) => {
            let value: i128 = (*value).into();
            write!(output, "{value}")
        }
        CborValue::Float(value) => write!(output, "{value}"),
        CborValue::Text(value) => output.write_str(value),
        CborValue::Bytes(value) => write!(output, "<{} bytes>", value.len()),
        CborValue::Array(values) => {
            let separator = if values
                .iter()
                .any(|value| matches!(value, CborValue::Map(_)))
            {
                "\n\n"
            } else {
                "\n"
            };
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.write_str(separator)?;
                }
                if matches!(value, CborValue::Map(_)) {
                    let mut trimmed = TrailingNewlineTrimmer::new(output);
                    write_cbor_tool_response_text(value, cache, &mut trimmed)?;
                } else {
                    write_cbor_tool_response_text(value, cache, output)?;
                }
            }
            Ok(())
        }
        CborValue::Map(entries) => write_cbor_tool_response_map(entries, None, cache, output),
        CborValue::Tag(_, inner) => write_cbor_tool_response_text(inner, cache, output),
        _ => Ok(()),
    }
}

/// Sink that delays literal newlines so array map items can discard only their
/// trailing newline run without buffering the complete item.
struct TrailingNewlineTrimmer<'a> {
    /// Downstream canonical text sink.
    output: &'a mut dyn fmt::Write,
    /// Delayed trailing newline count.
    trailing_newlines: usize,
}

impl<'a> TrailingNewlineTrimmer<'a> {
    fn new(output: &'a mut dyn fmt::Write) -> Self {
        Self {
            output,
            trailing_newlines: 0,
        }
    }
}

impl fmt::Write for TrailingNewlineTrimmer<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for character in text.chars() {
            if character == '\n' {
                self.trailing_newlines = self.trailing_newlines.saturating_add(1);
                continue;
            }
            for _ in 0..self.trailing_newlines {
                self.output.write_char('\n')?;
            }
            self.trailing_newlines = 0;
            self.output.write_char(character)?;
        }
        Ok(())
    }
}

/// One tool result item in the prompt timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultItem {
    /// Tool call this result answers.
    pub call_id: ToolCallId,
    /// Kind of tool that produced the result.
    pub tool_type: ToolType,
    /// Terminal status of the tool call.
    pub status: ToolResultStatus,
    /// Provider-facing rendered tool response plus raw payload.
    pub output: ToolResponse,
    /// Harness-owned provider-presentation authority retained across
    /// compaction and resume.
    #[serde(default)]
    pub presentation: crate::ToolResultPresentation,
    /// Ordered typed content appended after the normalized text output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_content: Vec<ToolResultContentPart>,
}

impl ToolResultItem {
    /// Renders the common base text provider adapters use for this terminal
    /// status.
    #[must_use]
    pub fn render_provider_text(&self) -> String {
        match &self.status {
            ToolResultStatus::Success => self.output.render(),
            ToolResultStatus::Error { message } => {
                let mut response = self.output.clone();
                response.headers.insert(
                    0,
                    ToolResponseHeader {
                        key: "error".to_owned(),
                        value: message.clone(),
                    },
                );
                response.render()
            }
            ToolResultStatus::Cancelled { reason } => ToolResponse {
                raw: CborValue::Null,
                headers: vec![ToolResponseHeader {
                    key: "cancelled".to_owned(),
                    value: reason.clone(),
                }],
                body: String::new(),
            }
            .render(),
        }
    }
}

/// Whether displayable reasoning text is a provider-summarized view or the
/// full reasoning text exposed by a compatible backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningTextKind {
    /// Provider-supplied summary intended for user display, not provider
    /// replay.
    Summary,
    /// Full reasoning text from a backend that expects it to be replayed as
    /// reasoning content rather than normal assistant text.
    Full,
}

/// Displayable reasoning text captured in an assistant output timeline.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReasoningTextItem {
    /// Whether this text is a summary or full backend reasoning content.
    pub kind: ReasoningTextKind,
    /// Accumulated reasoning text.
    pub text: String,
}

/// Private extension-to-harness output carrying one raw, untrusted local
/// compaction narrative.
///
/// The DTO itself does not establish bounds or validity. The harness must
/// validate and consume this control envelope before persisting a replacement
/// window.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LocalCompactionNarrativeItem {
    /// Raw untrusted narrative returned by the local compactor.
    pub narrative: String,
}

/// Maximum raw UTF-8 bytes accepted from one local compaction narrative.
pub const LOCAL_COMPACTION_NARRATIVE_MAX_BYTES: usize = 256 * 1024;

/// One item in Tau's prompt/response timeline.
#[derive(Clone, Debug, PartialEq)]
pub enum ContextItem {
    /// Message authored by a system, developer, user, or assistant role.
    Message(MessageItem),
    /// Assistant request to invoke a tool.
    ToolCall(ToolCallItem),
    /// Tool result returned to the model.
    ToolResult(ToolResultItem),
    /// Displayable reasoning text captured from the provider.
    ReasoningText(ReasoningTextItem),
    /// Private local-compactor terminal consumed only by the harness.
    LocalCompactionNarrative(LocalCompactionNarrativeItem),
    /// Provider-specific reasoning item used for backend replay.
    Reasoning(OpaqueProviderItem),
    /// User- or harness-authored request for the provider to compact context.
    CompactionTrigger,
    /// Provider-specific compaction item.
    Compaction(OpaqueProviderItem),
    /// Provider item that Tau does not yet understand.
    UnknownProviderItem(OpaqueProviderItem),
}

#[derive(Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum ContextItemRef<'a> {
    Message(&'a MessageItem),
    ToolCall(&'a ToolCallItem),
    ToolResult(&'a ToolResultItem),
    ReasoningText(&'a ReasoningTextItem),
    LocalCompactionNarrative(&'a LocalCompactionNarrativeItem),
    Reasoning(&'a OpaqueProviderItem),
    CompactionTrigger,
    Compaction(&'a OpaqueProviderItem),
    UnknownProviderItem(&'a OpaqueProviderItem),
}

impl Serialize for ContextItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let repr = match self {
            Self::Message(item) => ContextItemRef::Message(item),
            Self::ToolCall(item) => ContextItemRef::ToolCall(item),
            Self::ToolResult(item) => ContextItemRef::ToolResult(item),
            Self::ReasoningText(item) => ContextItemRef::ReasoningText(item),
            Self::LocalCompactionNarrative(item) => ContextItemRef::LocalCompactionNarrative(item),
            Self::Reasoning(item) => {
                item.validate_kind(OpaqueProviderItemKind::Reasoning)
                    .map_err(S::Error::custom)?;
                ContextItemRef::Reasoning(item)
            }
            Self::CompactionTrigger => ContextItemRef::CompactionTrigger,
            Self::Compaction(item) => {
                item.validate_kind(OpaqueProviderItemKind::Compaction)
                    .map_err(S::Error::custom)?;
                ContextItemRef::Compaction(item)
            }
            Self::UnknownProviderItem(item) => {
                item.validate_kind(OpaqueProviderItemKind::Unknown)
                    .map_err(S::Error::custom)?;
                ContextItemRef::UnknownProviderItem(item)
            }
        };
        repr.serialize(serializer)
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum ContextItemRepr {
    Message(MessageItem),
    ToolCall(ToolCallItem),
    ToolResult(ToolResultItem),
    ReasoningText(ReasoningTextItem),
    LocalCompactionNarrative(LocalCompactionNarrativeItem),
    Reasoning(OpaqueProviderItem),
    CompactionTrigger,
    Compaction(OpaqueProviderItem),
    UnknownProviderItem(OpaqueProviderItem),
}

impl<'de> Deserialize<'de> for ContextItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let item = match ContextItemRepr::deserialize(deserializer)? {
            ContextItemRepr::Message(item) => Self::Message(item),
            ContextItemRepr::ToolCall(item) => Self::ToolCall(item),
            ContextItemRepr::ToolResult(item) => Self::ToolResult(item),
            ContextItemRepr::ReasoningText(item) => Self::ReasoningText(item),
            ContextItemRepr::LocalCompactionNarrative(item) => Self::LocalCompactionNarrative(item),
            ContextItemRepr::Reasoning(item) => {
                item.validate_kind(OpaqueProviderItemKind::Reasoning)
                    .map_err(de::Error::custom)?;
                Self::Reasoning(item)
            }
            ContextItemRepr::CompactionTrigger => Self::CompactionTrigger,
            ContextItemRepr::Compaction(item) => {
                item.validate_kind(OpaqueProviderItemKind::Compaction)
                    .map_err(de::Error::custom)?;
                Self::Compaction(item)
            }
            ContextItemRepr::UnknownProviderItem(item) => {
                item.validate_kind(OpaqueProviderItemKind::Unknown)
                    .map_err(de::Error::custom)?;
                Self::UnknownProviderItem(item)
            }
        };
        Ok(item)
    }
}

/// Validates a standalone compaction replacement window before it can erase
/// older transcript history.
///
/// Provider-authored opaque items remain allowed for forward compatibility, but
/// harness-authored triggers/boundaries and structurally incomplete tool rounds
/// are rejected.
pub fn validate_compaction_window(items: &[ContextItem]) -> Result<(), &'static str> {
    validate_compaction_window_items(items)
}

/// A replacement window whose structural safety was checked before standalone
/// compaction can install it.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedCompactionWindow(Vec<ContextItem>);

impl ValidatedCompactionWindow {
    /// Validate `items` and retain the exact ordered provider window on
    /// success.
    pub fn new(items: Vec<ContextItem>) -> Result<Self, &'static str> {
        validate_compaction_window_items(&items)?;
        Ok(Self(items))
    }

    /// Consume this proof and return the exact validated replacement window.
    #[must_use]
    pub fn into_items(self) -> Vec<ContextItem> {
        self.0
    }

    /// Borrow the exact validated replacement items for an additional
    /// transaction-specific acceptance check.
    #[must_use]
    pub fn items(&self) -> &[ContextItem] {
        &self.0
    }
}

/// Applies the exhaustive per-item replacement-window policy.
fn validate_compaction_window_items(items: &[ContextItem]) -> Result<(), &'static str> {
    use std::collections::HashSet;

    if items.is_empty() {
        return Err("replacement window is empty");
    }
    let mut calls = HashSet::new();
    let mut results = HashSet::new();
    for item in items {
        match item {
            ContextItem::Message(message) if message.content.is_empty() => {
                return Err("replacement message has no content");
            }
            ContextItem::ToolCall(call) => {
                if call.call_id.as_str().is_empty() || call.name.as_str().is_empty() {
                    return Err("replacement tool call has an empty id or name");
                }
                if !calls.insert(call.call_id.clone()) {
                    return Err("replacement window has a duplicate tool call id");
                }
            }
            ContextItem::ToolResult(result) => {
                if result.call_id.as_str().is_empty() {
                    return Err("replacement tool result has an empty call id");
                }
                if !calls.contains(&result.call_id) {
                    return Err("replacement tool result has no preceding call");
                }
                if !results.insert(result.call_id.clone()) {
                    return Err("replacement window has a duplicate tool result");
                }
            }
            ContextItem::CompactionTrigger => {
                return Err("replacement window contains a harness compaction trigger");
            }
            ContextItem::LocalCompactionNarrative(_) => {
                return Err("replacement window contains a private local compaction envelope");
            }
            ContextItem::Message(_)
            | ContextItem::ReasoningText(_)
            | ContextItem::Reasoning(_)
            | ContextItem::Compaction(_)
            | ContextItem::UnknownProviderItem(_) => {}
        }
    }
    if calls != results {
        return Err("replacement window has a dangling tool call");
    }
    Ok(())
}

#[cfg(test)]
mod tests;

/// Materialized provider prompt context grouped into semantic blocks.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptContext {
    /// Ordered semantic blocks that make up the effective prompt history.
    pub blocks: Vec<ContextBlock>,
}

impl PromptContext {
    /// Iterates over the provider-visible item timeline.
    pub fn flatten_iter(&self) -> impl Iterator<Item = ContextItem> + '_ {
        fn context_block_items(block: &ContextBlock) -> ContextBlockItems<'_> {
            match block {
                ContextBlock::UserInput(block) => ContextBlockItems::Context(block.items.iter()),
                ContextBlock::AssistantResponse(block) => {
                    ContextBlockItems::Context(block.output_items.iter())
                }
                ContextBlock::ToolResults(block) => {
                    ContextBlockItems::ToolResult(block.items.iter())
                }
            }
        }

        enum ContextBlockItems<'a> {
            Context(std::slice::Iter<'a, ContextItem>),
            ToolResult(std::slice::Iter<'a, ToolResultItem>),
        }

        impl Iterator for ContextBlockItems<'_> {
            type Item = ContextItem;

            fn next(&mut self) -> Option<Self::Item> {
                match self {
                    ContextBlockItems::Context(iter) => iter.next().cloned(),
                    ContextBlockItems::ToolResult(iter) => {
                        iter.next().cloned().map(ContextItem::ToolResult)
                    }
                }
            }
        }

        self.blocks.iter().flat_map(context_block_items)
    }

    /// Flattens all blocks into the provider-visible item timeline.
    #[must_use]
    pub fn flatten(&self) -> Vec<ContextItem> {
        self.flatten_iter().collect()
    }

    /// Replaces typed provider image bytes with empty shared buffers while
    /// retaining safe metadata and transcript structure.
    ///
    /// This is for incidental diagnostics and generic projections. Provider
    /// paths, including cache-aligned local compaction, retain canonical bytes.
    pub fn clear_provider_image_bytes(&mut self) {
        for block in &mut self.blocks {
            match block {
                ContextBlock::UserInput(block) => {
                    clear_context_items_provider_image_bytes(&mut block.items);
                }
                ContextBlock::AssistantResponse(block) => {
                    clear_context_items_provider_image_bytes(&mut block.output_items);
                }
                ContextBlock::ToolResults(block) => {
                    for result in &mut block.items {
                        clear_tool_result_provider_image_bytes(result);
                    }
                }
            }
        }
    }
}

/// Replaces typed provider image bytes in a context-item slice with empty
/// shared buffers while retaining safe metadata.
pub fn clear_context_items_provider_image_bytes(items: &mut [ContextItem]) {
    for item in items {
        if let ContextItem::ToolResult(result) = item {
            clear_tool_result_provider_image_bytes(result);
        }
    }
}

/// Replaces typed provider image bytes in one tool result with empty shared
/// buffers while retaining safe metadata.
pub fn clear_tool_result_provider_image_bytes(result: &mut ToolResultItem) {
    for part in &mut result.provider_content {
        let ToolResultContentPart::Image(image) = part;
        image.data = Arc::from([]);
    }
}

/// One semantic block in a materialized provider prompt context.
#[expect(
    clippy::large_enum_variant,
    reason = "wire DTO variants stay inline to preserve the established public API"
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ContextBlock {
    /// User- or harness-authored input items.
    UserInput(UserInputBlock),
    /// One assistant response accepted from a provider.
    AssistantResponse(AssistantResponseBlock),
    /// Terminal tool results for one tool round.
    ToolResults(ToolResultsBlock),
}

/// Context block containing user input context items.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserInputBlock {
    /// Context items that make up the user input.
    pub items: Vec<ContextItem>,
}

/// Context block containing one assistant response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantResponseBlock {
    /// Provider response id, when the backend returned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_response_id: Option<String>,
    /// Provider backend that produced the response, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<ProviderBackend>,
    /// Output items produced by the assistant.
    pub output_items: Vec<ContextItem>,
    /// Provider token usage for this response, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ProviderTokenUsage>,
}

/// Context block containing tool results.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultsBlock {
    /// Tool result items in this block.
    pub items: Vec<ToolResultItem>,
}

/// Assistant-message phase label, mirroring the OpenAI Codex
/// `phase` field on assistant `message` items.
///
/// The Codex Responses API attaches one of these to each assistant
/// turn it produces (on models that support it, currently
/// `gpt-5.3-codex` and later). Resending the same value on later
/// turns lets the model distinguish intermediate progress from
/// completed work — the doc-recommended remedy for "early stopping"
/// in long, tool-heavy runs.
///
/// We capture the value off the SSE stream, persist it alongside the
/// assistant turn, and echo it back on every re-serialized history
/// replay. Older models that do not emit this field still receive
/// the `final_answer` default on assistant message items the harness
/// re-serializes, which is the explicit guidance in the deployment
/// checklist.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    /// Intermediate progress / preliminary notes.
    Commentary,
    /// Final completed response.
    FinalAnswer,
}

impl MessagePhase {
    /// Wire string accepted by the OpenAI Codex Responses API on
    /// assistant `message` items.
    #[must_use]
    pub const fn as_openai_wire(self) -> &'static str {
        match self {
            Self::Commentary => "commentary",
            Self::FinalAnswer => "final_answer",
        }
    }
}

/// A tool definition available for the agent to use.
///
/// This is outbound (harness → LLM in the prompt), so the harness
/// controls the string and we enforce the `ToolName` invariant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Protocol tool name used for calls and results.
    pub name: ToolName,
    /// Optional provider-visible tool name when it differs from the protocol
    /// name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_visible_name: Option<ToolName>,
    /// Optional model-visible tool description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether this is a JSON-schema function tool or a freeform custom tool.
    pub tool_type: ToolType,
    /// JSON Schema describing the tool's input parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    /// Optional freeform/custom input format. `None` means provider-default
    /// unconstrained text for custom tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ToolFormat>,
}
