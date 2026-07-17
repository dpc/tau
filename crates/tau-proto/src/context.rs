//! Provider context and transcript item support types.
//!
//! Semantic fields and provider replay sidecars follow
//! `SPEC-tau-proto-provider-data`.

use std::fmt::{self, Write as _};
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

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
}

/// Opaque provider-owned payload preserved without semantic authority.
#[derive(Clone, Debug, PartialEq)]
pub struct OpaqueProviderItem {
    /// Parsed provider item for semantic inspection and legacy replay.
    pub value: CborValue,
    /// Raw provider item JSON used for cache-identity-preserving replay.
    ///
    /// This sidecar is provider-visible syntax only. Consumers that need to
    /// inspect, validate, or make semantic decisions must use [`Self::value`].
    pub raw_json: Option<String>,
}

impl OpaqueProviderItem {
    /// Builds an opaque provider item from a parsed CBOR value.
    #[must_use]
    pub fn new(value: CborValue) -> Self {
        Self {
            value,
            raw_json: None,
        }
    }

    /// Builds an opaque provider item with a raw JSON replay sidecar.
    #[must_use]
    pub fn with_raw_json(value: CborValue, raw_json: impl Into<String>) -> Self {
        Self {
            value,
            raw_json: Some(raw_json.into()),
        }
    }
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
            #[serde(skip_serializing_if = "Option::is_none")]
            raw_json: Option<&'a str>,
        }

        Repr {
            tau_opaque_provider_item_version: 1,
            value: &self.value,
            raw_json: self.raw_json.as_deref(),
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
            #[serde(default)]
            raw_json: Option<String>,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Current(Current),
            Legacy(CborValue),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Current(Current {
                tau_opaque_provider_item_version: 1,
                value,
                raw_json,
            }) => Ok(Self { value, raw_json }),
            Repr::Current(Current {
                tau_opaque_provider_item_version,
                ..
            }) => Err(de::Error::custom(format!(
                "unsupported opaque provider item version {tau_opaque_provider_item_version}"
            ))),
            Repr::Legacy(value) => Ok(Self::new(value)),
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

mod arc_bytes {
    use std::sync::Arc;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S>(data: &Arc<[u8]>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serde_bytes::Bytes::new(data).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Arc<[u8]>, D::Error>
    where
        D: Deserializer<'de>,
    {
        serde_bytes::ByteBuf::deserialize(deserializer).map(|bytes| Arc::from(bytes.into_vec()))
    }
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
        for header in &self.headers {
            out.push_str(&sanitize_provider_header_text(&header.key));
            out.push_str(": ");
            out.push_str(&sanitize_provider_header_text(&header.value));
            out.push('\n');
        }
        if !self.headers.is_empty() {
            out.push('\n');
        }
        out.push_str(&sanitize_provider_body_text(&self.body));
        out
    }

    fn from_cbor_map(entries: &[(CborValue, CborValue)]) -> Self {
        let has_output = entries.iter().any(|(key, _)| {
            matches!(key, CborValue::Text(key) if key == "output" || key == "line-numbered content")
        });
        let raw = CborValue::Map(entries.to_vec());
        let mut headers = Vec::new();
        let mut body_parts = Vec::new();
        for (key, value) in entries {
            let key = cbor_tool_response_text(key);
            if has_output && key == "data" {
                continue;
            }
            let value = cbor_tool_response_text(value);
            if key == "output" || key == "line-numbered content" {
                body_parts.push(value);
            } else if value.contains('\n') {
                let key = sanitize_provider_header_text(&key);
                body_parts.push(format!("{key}:\n{value}"));
            } else {
                headers.push(ToolResponseHeader { key, value });
            }
        }
        Self {
            raw,
            headers,
            body: body_parts.join("\n"),
        }
    }
}

fn sanitize_provider_header_text(input: &str) -> String {
    sanitize_provider_text(input, ProviderTextMode::Header)
}

fn sanitize_provider_body_text(input: &str) -> String {
    sanitize_provider_text(input, ProviderTextMode::Body)
}

#[derive(Clone, Copy)]
enum ProviderTextMode {
    Header,
    Body,
}

fn sanitize_provider_text(input: &str, mode: ProviderTextMode) -> String {
    let mut output = String::new();
    for ch in input.chars() {
        match ch {
            '\n' if matches!(mode, ProviderTextMode::Body) => output.push('\n'),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\0' => output.push_str("\\0"),
            '\u{1b}' => output.push_str("\\x1b"),
            '\u{2028}' => output.push_str("\\u{2028}"),
            '\u{2029}' => output.push_str("\\u{2029}"),
            ch if is_provider_unsafe_control(ch) => {
                write!(output, "\\u{{{:x}}}", ch as u32).expect("writing to String cannot fail");
            }
            ch => output.push(ch),
        }
    }
    output
}

fn is_provider_unsafe_control(ch: char) -> bool {
    matches!(ch, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}')
}

fn cbor_tool_response_text(value: &CborValue) -> String {
    match value {
        CborValue::Null => String::new(),
        CborValue::Bool(b) => b.to_string(),
        CborValue::Integer(i) => {
            let n: i128 = (*i).into();
            n.to_string()
        }
        CborValue::Float(f) => f.to_string(),
        CborValue::Text(s) => s.clone(),
        CborValue::Bytes(b) => format!("<{} bytes>", b.len()),
        CborValue::Array(arr) => {
            let separator = if arr.iter().any(|value| matches!(value, CborValue::Map(_))) {
                "\n\n"
            } else {
                "\n"
            };
            arr.iter()
                .map(|item| {
                    let text = cbor_tool_response_text(item);
                    if matches!(item, CborValue::Map(_)) {
                        text.trim_end_matches('\n').to_owned()
                    } else {
                        text
                    }
                })
                .collect::<Vec<_>>()
                .join(separator)
        }
        CborValue::Map(entries) => ToolResponse::from_cbor_map(entries).render(),
        CborValue::Tag(_, inner) => cbor_tool_response_text(inner),
        _ => String::new(),
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
    /// Ordered typed content appended after the normalized text output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_content: Vec<ToolResultContentPart>,
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

/// One item in Tau's prompt/response timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ContextItem {
    /// Message authored by a system, developer, user, or assistant role.
    Message(MessageItem),
    /// Assistant request to invoke a tool.
    ToolCall(ToolCallItem),
    /// Tool result returned to the model.
    ToolResult(ToolResultItem),
    /// Displayable reasoning text captured from the provider.
    ReasoningText(ReasoningTextItem),
    /// Provider-specific reasoning item used for backend replay.
    Reasoning(OpaqueProviderItem),
    /// User- or harness-authored request for the provider to compact context.
    CompactionTrigger,
    /// Provider-specific compaction item.
    Compaction(OpaqueProviderItem),
    /// Provider item that Tau does not yet understand.
    UnknownProviderItem(OpaqueProviderItem),
}

/// Validates a standalone compaction replacement window before it can erase
/// older transcript history.
///
/// Unknown provider items remain allowed for forward compatibility, but
/// harness-authored triggers/boundaries and structurally incomplete tool rounds
/// are rejected.
pub fn validate_compaction_window(items: &[ContextItem]) -> Result<(), &'static str> {
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
            ContextItem::CompactionTrigger | ContextItem::Compaction(_) => {
                return Err("replacement window contains a compaction control item");
            }
            ContextItem::Message(_)
            | ContextItem::ReasoningText(_)
            | ContextItem::Reasoning(_)
            | ContextItem::UnknownProviderItem(_) => {}
        }
    }
    if calls != results {
        return Err("replacement window has a dangling tool call");
    }
    Ok(())
}

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
    /// This is for incidental diagnostics and generic projections only. Durable
    /// transcript and provider-directed paths must retain the canonical bytes.
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
