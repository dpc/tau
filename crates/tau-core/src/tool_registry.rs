//! Live tool registration tracking and routing of `tool.request` events
//! to the connection that owns each tool.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use tau_proto::{
    CborValue, ConnectionId, PromptFragment, ToolExample, ToolExampleSelector, ToolGroup, ToolName,
    ToolRegister, ToolRequest, ToolSpec, ToolStarted, ToolType, nearest_name_suggestion,
};

use crate::connection::RouteError;

const MAX_DIAGNOSTIC_ITEMS: usize = 16;
const MAX_DIAGNOSTIC_ITEM_CHARS: usize = 40;
const MAX_DIAGNOSTIC_MESSAGE_CHARS: usize = 1024;
const MAX_DIAGNOSTIC_PATH_CHARS: usize = 200;
const MAX_TOOL_EXAMPLES: usize = 32;
const MAX_TOOL_EXAMPLE_ID_CHARS: usize = 64;
const MAX_TOOL_EXAMPLE_TEXT_CHARS: usize = 120;
const MAX_TOOL_EXAMPLE_SELECTOR_PATH: usize = 8;
const MAX_TOOL_EXAMPLE_ARGUMENT_CHARS: usize = 600;
const MAX_TOOL_EXAMPLE_ARGUMENT_NODES: usize = 128;
const MAX_TOOL_EXAMPLE_HINT_CHARS: usize = 1200;

/// Kind of provider registered for a tool name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolProviderKind {
    /// Tool is handled inside the harness after `tool.started` is published.
    Internal,
    /// Tool is handled by the extension connection that registered it.
    Extension,
}

/// One live provider registered for a tool name.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolProvider {
    /// Connection that registered this tool, or a stable synthetic id for an
    /// internal harness-owned provider.
    pub connection_id: ConnectionId,
    /// Whether this provider is internal to the harness or extension-owned.
    pub kind: ToolProviderKind,
    /// Tool metadata advertised to the model and used for routing.
    pub tool: ToolSpec,
    /// Optional group this tool belongs to.
    pub tool_group: Option<ToolGroup>,
    /// Optional prompt fragment template contributed while this tool is
    /// enabled.
    pub prompt_fragment: Option<PromptFragment>,
}

/// Warning emitted by the tool registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRegistryWarning {
    DuplicateRegistration {
        tool_name: ToolName,
        existing_provider_ids: Vec<ConnectionId>,
    },
}

/// Error emitted by the tool registry while rejecting a bad registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRegistrationError {
    InvalidExample {
        tool_name: ToolName,
        example_id: String,
        reason: String,
    },
}

impl fmt::Display for ToolRegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidExample {
                tool_name,
                example_id,
                reason,
            } => write!(
                f,
                "invalid example `{example_id}` for tool `{tool_name}`: {reason}"
            ),
        }
    }
}

impl Error for ToolRegistrationError {}

/// Summary of one registration call.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RegisterToolReport {
    pub warnings: Vec<ToolRegistryWarning>,
    pub errors: Vec<ToolRegistrationError>,
}

/// Error returned when a tool tool request cannot be routed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRouteError {
    NoProvider { tool_name: ToolName },
    Route(RouteError),
}

/// Error returned when a tool call's arguments do not match its JSON schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolArgumentValidationError {
    path: String,
    message: String,
}

impl ToolArgumentValidationError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            path: bounded_text(&path.into(), MAX_DIAGNOSTIC_PATH_CHARS),
            message: bounded_text(&message, MAX_DIAGNOSTIC_MESSAGE_CHARS),
        }
    }
}

impl fmt::Display for ToolArgumentValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path == "$" {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl Error for ToolArgumentValidationError {}

impl fmt::Display for ToolRouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProvider { tool_name } => write!(f, "no live provider for tool: {tool_name}"),
            Self::Route(error) => write!(f, "failed to route tool tool request: {error}"),
        }
    }
}

impl Error for ToolRouteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NoProvider { .. } => None,
            Self::Route(error) => Some(error),
        }
    }
}

/// Destination selected for a routed tool request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRouteTarget {
    /// Harness-owned tool. No extension connection owns the call; the harness
    /// handles the later `tool.started` event itself.
    Internal,
    /// Extension connection that registered the selected tool.
    Extension(ConnectionId),
}

/// Summary of one `tool.request` routing decision.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolRouteReport {
    /// Selected destination for the accepted request.
    pub target: ToolRouteTarget,
    pub invoke: ToolStarted,
}

/// Validates a model-produced function-tool argument object against the tool's
/// JSON Schema parameters.
///
/// Tau tool schemas intentionally use a small JSON Schema subset: object
/// properties, required fields, closed objects via `additionalProperties:
/// false`, primitive `type`, `enum`, array `items`, and numeric/string/array
/// bounds. Unknown schema keywords are ignored so richer third-party schemas do
/// not become harness errors.
pub fn validate_tool_arguments(
    tool: &ToolSpec,
    arguments: &CborValue,
) -> Result<(), ToolArgumentValidationError> {
    if !matches!(tool.tool_type, ToolType::Function) {
        return Ok(());
    }
    let Some(schema) = tool.parameters.as_ref() else {
        return Ok(());
    };
    validate_json_schema(schema, arguments, "$")
}

/// Validates provider-owned examples attached to a tool registration.
pub fn validate_tool_examples(tool: &ToolSpec) -> Result<(), ToolRegistrationError> {
    if tool.examples.len() > MAX_TOOL_EXAMPLES {
        return Err(invalid_example(
            tool,
            "<tool>",
            format!("too many examples; maximum is {MAX_TOOL_EXAMPLES}"),
        ));
    }

    let mut seen_ids = std::collections::HashSet::new();
    for example in &tool.examples {
        validate_tool_example(tool, example, &mut seen_ids)?;
    }
    Ok(())
}

fn validate_tool_example(
    tool: &ToolSpec,
    example: &ToolExample,
    seen_ids: &mut std::collections::HashSet<String>,
) -> Result<(), ToolRegistrationError> {
    if example.id.trim().is_empty() {
        return Err(invalid_example(tool, "<empty>", "id must not be empty"));
    }
    if example.id.chars().count() > MAX_TOOL_EXAMPLE_ID_CHARS {
        return Err(invalid_example(tool, &example.id, "id is too long"));
    }
    if !seen_ids.insert(example.id.clone()) {
        return Err(invalid_example(tool, &example.id, "duplicate id"));
    }
    for (field, value) in [
        ("title", example.title.as_deref()),
        ("note", example.note.as_deref()),
    ] {
        if value.is_some_and(|value| value.chars().count() > MAX_TOOL_EXAMPLE_TEXT_CHARS) {
            return Err(invalid_example(
                tool,
                &example.id,
                format!("{field} is too long"),
            ));
        }
    }
    if let Some(selector) = &example.subcommand {
        validate_tool_example_selector(tool, example, selector)?;
    }
    if !cbor_value_within_budget(
        &example.arguments,
        MAX_TOOL_EXAMPLE_ARGUMENT_CHARS,
        MAX_TOOL_EXAMPLE_ARGUMENT_NODES,
    ) {
        return Err(invalid_example(
            tool,
            &example.id,
            "arguments are too large for a compact example",
        ));
    }
    validate_tool_arguments(tool, &example.arguments)
        .map_err(|error| invalid_example(tool, &example.id, error.to_string()))
}

fn validate_tool_example_selector(
    tool: &ToolSpec,
    example: &ToolExample,
    selector: &ToolExampleSelector,
) -> Result<(), ToolRegistrationError> {
    if selector.path.is_empty() {
        return Err(invalid_example(
            tool,
            &example.id,
            "subcommand selector path must not be empty",
        ));
    }
    if selector.path.len() > MAX_TOOL_EXAMPLE_SELECTOR_PATH {
        return Err(invalid_example(
            tool,
            &example.id,
            "subcommand selector path is too long",
        ));
    }
    if selector.path.iter().any(|segment| {
        segment.trim().is_empty() || segment.chars().count() > MAX_DIAGNOSTIC_ITEM_CHARS
    }) {
        return Err(invalid_example(
            tool,
            &example.id,
            "subcommand selector path contains an invalid segment",
        ));
    }
    match cbor_path_value(&example.arguments, &selector.path) {
        Some(value) if value == &selector.value => Ok(()),
        Some(_) => Err(invalid_example(
            tool,
            &example.id,
            "subcommand selector value does not match example arguments",
        )),
        None => Err(invalid_example(
            tool,
            &example.id,
            "subcommand selector path is absent from example arguments",
        )),
    }
}

fn invalid_example(
    tool: &ToolSpec,
    example_id: impl Into<String>,
    reason: impl Into<String>,
) -> ToolRegistrationError {
    ToolRegistrationError::InvalidExample {
        tool_name: tool.name.clone(),
        example_id: bounded_text(&example_id.into(), MAX_TOOL_EXAMPLE_ID_CHARS),
        reason: bounded_text(&reason.into(), MAX_DIAGNOSTIC_MESSAGE_CHARS),
    }
}

/// Selects and renders one compact model-visible repair hint for a failed call.
pub fn tool_example_hint(tool: &ToolSpec, failed_arguments: &CborValue) -> Option<String> {
    let example = select_tool_example(tool, failed_arguments)?;
    let mut hint = String::from("\n\nExample valid call");
    if let Some(title) = example.title.as_deref() {
        hint.push_str(": ");
        hint.push_str(&bounded_text(title, MAX_TOOL_EXAMPLE_TEXT_CHARS));
    }
    hint.push_str(":\n");
    hint.push_str(&bounded_text(
        &render_cbor_json(&example.arguments),
        MAX_TOOL_EXAMPLE_HINT_CHARS / 2,
    ));
    if let Some(note) = example.note.as_deref() {
        hint.push_str("\nNote: ");
        hint.push_str(&bounded_text(note, MAX_TOOL_EXAMPLE_TEXT_CHARS));
    }
    if let Some(allowed) = selector_allowed_values(tool, failed_arguments) {
        hint.push_str("\nSubcommand values include: ");
        hint.push_str(&allowed);
    }
    Some(bounded_text(&hint, MAX_TOOL_EXAMPLE_HINT_CHARS))
}

fn select_tool_example<'a>(
    tool: &'a ToolSpec,
    failed_arguments: &CborValue,
) -> Option<&'a ToolExample> {
    if tool.examples.is_empty() {
        return None;
    }
    let mut examples = tool.examples.iter().collect::<Vec<_>>();
    examples.sort_by(|a, b| a.id.cmp(&b.id));

    let matching_subcommand = examples.iter().copied().find(|example| {
        let Some(selector) = &example.subcommand else {
            return false;
        };
        cbor_path_value(failed_arguments, &selector.path) == Some(&selector.value)
    });
    matching_subcommand
        .or_else(|| {
            examples
                .iter()
                .copied()
                .find(|example| example.subcommand.is_none())
        })
        .or_else(|| examples.first().copied())
}

fn selector_allowed_values(tool: &ToolSpec, failed_arguments: &CborValue) -> Option<String> {
    if tool.examples.iter().any(|example| {
        example.subcommand.as_ref().is_some_and(|selector| {
            cbor_path_value(failed_arguments, &selector.path) == Some(&selector.value)
        })
    }) {
        return None;
    }
    let mut values = tool
        .examples
        .iter()
        .filter_map(|example| example.subcommand.as_ref())
        .map(|selector| short_cbor_value(&selector.value))
        .collect::<Vec<_>>();
    if values.is_empty() {
        None
    } else {
        values.sort();
        values.dedup();
        let total = values.len();
        let values = values.into_iter().take(MAX_DIAGNOSTIC_ITEMS).collect();
        Some(bounded_list(values, total))
    }
}

fn cbor_path_value<'a>(value: &'a CborValue, path: &[String]) -> Option<&'a CborValue> {
    let mut current = value;
    for segment in path {
        let CborValue::Map(entries) = current else {
            return None;
        };
        current = entries.iter().find_map(|(key, value)| match key {
            CborValue::Text(key) if key == segment => Some(value),
            _ => None,
        })?;
    }
    Some(current)
}

fn cbor_value_within_budget(value: &CborValue, char_budget: usize, node_budget: usize) -> bool {
    fn consume(value: &CborValue, chars: &mut usize, nodes: &mut usize) -> bool {
        let Some(remaining_nodes) = nodes.checked_sub(1) else {
            return false;
        };
        *nodes = remaining_nodes;
        match value {
            CborValue::Null => consume_chars(chars, 4),
            CborValue::Bool(value) => consume_chars(chars, if *value { 4 } else { 5 }),
            CborValue::Integer(value) => {
                let value: i128 = (*value).into();
                consume_chars(chars, value.to_string().len())
            }
            CborValue::Float(value) => consume_chars(chars, value.to_string().len()),
            CborValue::Text(value) => consume_bounded_text(chars, value),
            CborValue::Bytes(bytes) => consume_chars(chars, bytes.len().min(*chars + 1)),
            CborValue::Array(values) => values.iter().all(|value| consume(value, chars, nodes)),
            CborValue::Map(entries) => entries
                .iter()
                .all(|(key, value)| consume(key, chars, nodes) && consume(value, chars, nodes)),
            CborValue::Tag(_, value) => consume(value, chars, nodes),
            _ => false,
        }
    }

    fn consume_chars(remaining: &mut usize, len: usize) -> bool {
        let Some(next) = remaining.checked_sub(len) else {
            return false;
        };
        *remaining = next;
        true
    }

    fn consume_bounded_text(remaining: &mut usize, value: &str) -> bool {
        let len = value.chars().take(*remaining + 1).count();
        consume_chars(remaining, len)
    }

    let mut chars = char_budget;
    let mut nodes = node_budget;
    consume(value, &mut chars, &mut nodes)
}

fn render_cbor_json(value: &CborValue) -> String {
    serde_json::to_string(&cbor_to_json_value(value)).unwrap_or_else(|_| format!("{value:?}"))
}

fn cbor_to_json_value(value: &CborValue) -> serde_json::Value {
    match value {
        CborValue::Null => serde_json::Value::Null,
        CborValue::Bool(value) => serde_json::Value::Bool(*value),
        CborValue::Integer(value) => {
            let value: i128 = (*value).into();
            if let Ok(value) = i64::try_from(value) {
                serde_json::Value::Number(value.into())
            } else {
                serde_json::Value::String(value.to_string())
            }
        }
        CborValue::Float(value) => serde_json::Number::from_f64(*value)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        CborValue::Text(value) => serde_json::Value::String(value.clone()),
        CborValue::Array(values) => {
            serde_json::Value::Array(values.iter().map(cbor_to_json_value).collect())
        }
        CborValue::Map(entries) => serde_json::Value::Object(
            entries
                .iter()
                .filter_map(|(key, value)| match key {
                    CborValue::Text(key) => Some((key.clone(), cbor_to_json_value(value))),
                    _ => None,
                })
                .collect(),
        ),
        CborValue::Tag(_, value) => cbor_to_json_value(value),
        _ => serde_json::Value::Null,
    }
}

fn validate_json_schema(
    schema: &serde_json::Value,
    value: &CborValue,
    path: &str,
) -> Result<(), ToolArgumentValidationError> {
    match schema {
        serde_json::Value::Bool(true) => return Ok(()),
        serde_json::Value::Bool(false) => {
            return Err(ToolArgumentValidationError::new(
                path,
                "value is rejected by schema",
            ));
        }
        _ => {}
    }

    let Some(schema) = schema.as_object() else {
        return Ok(());
    };

    if let Some(type_schema) = schema.get("type")
        && !schema_type_matches(type_schema, value)
    {
        return Err(type_error(path, type_schema, value));
    }

    if let Some(enum_values) = schema.get("enum").and_then(serde_json::Value::as_array)
        && !enum_values
            .iter()
            .any(|allowed| tau_proto::json_to_cbor(allowed) == *value)
    {
        return Err(enum_error(path, enum_values, value));
    }

    match value {
        CborValue::Map(entries) => validate_object_schema(schema, entries, path),
        CborValue::Array(values) => validate_array_schema(schema, values, path),
        CborValue::Text(text) => validate_string_schema(schema, text, path),
        CborValue::Integer(_) | CborValue::Float(_) => validate_number_schema(schema, value, path),
        _ => Ok(()),
    }
}

fn schema_type_matches(type_schema: &serde_json::Value, value: &CborValue) -> bool {
    match type_schema {
        serde_json::Value::String(kind) => schema_type_name_matches(kind, value),
        serde_json::Value::Array(kinds) => kinds.iter().any(|kind| {
            kind.as_str()
                .is_some_and(|kind| schema_type_name_matches(kind, value))
        }),
        _ => true,
    }
}

fn schema_type_name_matches(kind: &str, value: &CborValue) -> bool {
    match kind {
        "object" => matches!(value, CborValue::Map(_)),
        "array" => matches!(value, CborValue::Array(_)),
        "string" => matches!(value, CborValue::Text(_)),
        "boolean" => matches!(value, CborValue::Bool(_)),
        "integer" => matches!(value, CborValue::Integer(_)),
        "number" => matches!(value, CborValue::Integer(_) | CborValue::Float(_)),
        "null" => matches!(value, CborValue::Null),
        _ => true,
    }
}

fn type_error(
    path: &str,
    type_schema: &serde_json::Value,
    value: &CborValue,
) -> ToolArgumentValidationError {
    let expected = match type_schema {
        serde_json::Value::String(kind) => kind.clone(),
        serde_json::Value::Array(kinds) => kinds
            .iter()
            .filter_map(serde_json::Value::as_str)
            .take(MAX_DIAGNOSTIC_ITEMS)
            .collect::<Vec<_>>()
            .join(" or "),
        _ => "expected schema type".to_owned(),
    };
    if path == "$" && expected == "object" {
        ToolArgumentValidationError::new(
            path,
            format!(
                "arguments must be an object; expected object, got {}",
                cbor_type_name(value)
            ),
        )
    } else {
        ToolArgumentValidationError::new(
            path,
            format!("expected {expected}, got {}", cbor_type_name(value)),
        )
    }
}

fn enum_error(
    path: &str,
    enum_values: &[serde_json::Value],
    value: &CborValue,
) -> ToolArgumentValidationError {
    let allowed = enum_values
        .iter()
        .take(MAX_DIAGNOSTIC_ITEMS)
        .map(short_json_value)
        .collect::<Vec<_>>();
    let allowed = bounded_list(allowed, enum_values.len());
    let mut message = format!(
        "invalid enum value {}; allowed values: {}",
        short_cbor_value(value),
        allowed
    );
    if let CborValue::Text(text) = value
        && let Some(suggestion) = nearest_name_suggestion(
            text,
            enum_values.iter().filter_map(serde_json::Value::as_str),
        )
    {
        message.push_str(&format!("; did you mean `{suggestion}`?"));
    }
    ToolArgumentValidationError::new(path, message)
}

fn validate_object_schema(
    schema: &serde_json::Map<String, serde_json::Value>,
    entries: &[(CborValue, CborValue)],
    path: &str,
) -> Result<(), ToolArgumentValidationError> {
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object);

    if let Some(required) = schema.get("required").and_then(serde_json::Value::as_array) {
        let missing = required
            .iter()
            .filter_map(serde_json::Value::as_str)
            .filter(|required_name| {
                !entries
                    .iter()
                    .any(|(key, _)| cbor_key_matches(key, required_name))
            })
            .take(MAX_DIAGNOSTIC_ITEMS + 1)
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(missing_required_error(path, &missing));
        }
    }

    if matches!(
        schema.get("additionalProperties"),
        Some(serde_json::Value::Bool(false))
    ) {
        let unknown = entries
            .iter()
            .filter_map(|(key, _)| match key {
                CborValue::Text(field_name)
                    if properties.is_none_or(|properties| !properties.contains_key(field_name)) =>
                {
                    Some(field_name.as_str())
                }
                _ => None,
            })
            .take(MAX_DIAGNOSTIC_ITEMS + 1)
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            let mut allowed = properties
                .into_iter()
                .flat_map(serde_json::Map::keys)
                .map(String::as_str)
                .take(MAX_DIAGNOSTIC_ITEMS + 1)
                .collect::<Vec<_>>();
            allowed.sort_unstable();
            return Err(unexpected_properties_error(path, &unknown, &allowed));
        }
    }

    for (key, field_value) in entries {
        let CborValue::Text(field_name) = key else {
            return Err(ToolArgumentValidationError::new(
                path,
                "object keys must be strings",
            ));
        };
        if let Some(field_schema) = properties.and_then(|properties| properties.get(field_name)) {
            validate_json_schema(field_schema, field_value, &child_path(path, field_name))?;
            continue;
        }
        match schema.get("additionalProperties") {
            Some(serde_json::Value::Bool(false)) => {}
            Some(additional_schema @ serde_json::Value::Object(_)) => {
                validate_json_schema(
                    additional_schema,
                    field_value,
                    &child_path(path, field_name),
                )?;
            }
            Some(serde_json::Value::Bool(true)) | None => {}
            Some(_) => {}
        }
    }

    Ok(())
}

fn cbor_key_matches(key: &CborValue, expected: &str) -> bool {
    matches!(key, CborValue::Text(text) if text == expected)
}

fn child_path(parent: &str, field: &str) -> String {
    let field = bounded_text(field, MAX_DIAGNOSTIC_ITEM_CHARS);
    if parent == "$" {
        format!("$.{field}")
    } else {
        format!("{parent}.{field}")
    }
}

fn item_path(parent: &str, index: usize) -> String {
    format!("{parent}[{index}]")
}

fn missing_required_error(path: &str, names: &[&str]) -> ToolArgumentValidationError {
    let total = names.len();
    let quoted = bounded_list(
        names
            .iter()
            .take(MAX_DIAGNOSTIC_ITEMS)
            .map(|name| format!("`{}`", bounded_text(name, MAX_DIAGNOSTIC_ITEM_CHARS)))
            .collect::<Vec<_>>(),
        total,
    );
    let noun = if path == "$" { "argument" } else { "property" };
    ToolArgumentValidationError::new(path, format!("missing required {noun}(s): {quoted}"))
}

fn unexpected_properties_error(
    path: &str,
    names: &[&str],
    allowed: &[&str],
) -> ToolArgumentValidationError {
    let quoted = bounded_list(
        names
            .iter()
            .take(MAX_DIAGNOSTIC_ITEMS)
            .map(|name| format!("`{}`", bounded_text(name, MAX_DIAGNOSTIC_ITEM_CHARS)))
            .collect::<Vec<_>>(),
        names.len(),
    );
    let allowed = if allowed.is_empty() {
        "none".to_owned()
    } else {
        bounded_list(
            allowed
                .iter()
                .take(MAX_DIAGNOSTIC_ITEMS)
                .map(|name| format!("`{}`", bounded_text(name, MAX_DIAGNOSTIC_ITEM_CHARS)))
                .collect::<Vec<_>>(),
            allowed.len(),
        )
    };
    let noun = if path == "$" { "argument" } else { "property" };
    ToolArgumentValidationError::new(
        path,
        format!("unexpected {noun}(s): {quoted}; allowed fields: {allowed}"),
    )
}

fn bounded_list(items: Vec<String>, total_items: usize) -> String {
    let shown = items.len().min(MAX_DIAGNOSTIC_ITEMS);
    let mut rendered = items.into_iter().take(shown).collect::<Vec<_>>();
    if shown < total_items {
        rendered.push("… and more".to_owned());
    }
    rendered.join(", ")
}

fn validate_array_schema(
    schema: &serde_json::Map<String, serde_json::Value>,
    values: &[CborValue],
    path: &str,
) -> Result<(), ToolArgumentValidationError> {
    if let Some(min_items) = schema
        .get("minItems")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        && values.len() < min_items
    {
        return Err(ToolArgumentValidationError::new(
            path,
            format!("must contain at least {min_items} item(s)"),
        ));
    }
    if let Some(max_items) = schema
        .get("maxItems")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        && max_items < values.len()
    {
        return Err(ToolArgumentValidationError::new(
            path,
            format!("must contain at most {max_items} item(s)"),
        ));
    }
    if let Some(item_schema) = schema.get("items") {
        for (idx, item) in values.iter().enumerate() {
            validate_json_schema(item_schema, item, &item_path(path, idx))?;
        }
    }
    Ok(())
}

fn validate_string_schema(
    schema: &serde_json::Map<String, serde_json::Value>,
    text: &str,
    path: &str,
) -> Result<(), ToolArgumentValidationError> {
    let len = text.chars().count();
    if let Some(min_len) = schema
        .get("minLength")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        && len < min_len
    {
        return Err(ToolArgumentValidationError::new(
            path,
            format!("must contain at least {min_len} character(s)"),
        ));
    }
    if let Some(max_len) = schema
        .get("maxLength")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        && max_len < len
    {
        return Err(ToolArgumentValidationError::new(
            path,
            format!("must contain at most {max_len} character(s)"),
        ));
    }
    Ok(())
}

fn validate_number_schema(
    schema: &serde_json::Map<String, serde_json::Value>,
    value: &CborValue,
    path: &str,
) -> Result<(), ToolArgumentValidationError> {
    let Some(number) = cbor_number_as_f64(value) else {
        return Ok(());
    };
    if let Some(minimum) = schema.get("minimum").and_then(serde_json::Value::as_f64)
        && number < minimum
    {
        return Err(ToolArgumentValidationError::new(
            path,
            format!("must be at least {minimum}"),
        ));
    }
    if let Some(maximum) = schema.get("maximum").and_then(serde_json::Value::as_f64)
        && maximum < number
    {
        return Err(ToolArgumentValidationError::new(
            path,
            format!("must be at most {maximum}"),
        ));
    }
    Ok(())
}

fn cbor_number_as_f64(value: &CborValue) -> Option<f64> {
    match value {
        CborValue::Integer(value) => {
            let value: i128 = (*value).into();
            Some(value as f64)
        }
        CborValue::Float(value) => Some(*value),
        _ => None,
    }
}

fn cbor_type_name(value: &CborValue) -> &'static str {
    match value {
        CborValue::Null => "null",
        CborValue::Bool(_) => "boolean",
        CborValue::Integer(_) => "integer",
        CborValue::Float(_) => "number",
        CborValue::Bytes(_) => "bytes",
        CborValue::Text(_) => "string",
        CborValue::Array(_) => "array",
        CborValue::Map(_) => "object",
        CborValue::Tag(_, _) => "tagged value",
        _ => "value",
    }
}

fn short_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => {
            format!("`{}`", bounded_text(text, MAX_DIAGNOSTIC_ITEM_CHARS))
        }
        _ => bounded_text(&value.to_string(), MAX_DIAGNOSTIC_ITEM_CHARS),
    }
}

fn short_cbor_value(value: &CborValue) -> String {
    match value {
        CborValue::Text(text) => {
            format!("`{}`", bounded_text(text, MAX_DIAGNOSTIC_ITEM_CHARS))
        }
        CborValue::Integer(value) => {
            let value: i128 = (*value).into();
            value.to_string()
        }
        CborValue::Float(value) => value.to_string(),
        CborValue::Bool(value) => value.to_string(),
        CborValue::Null => "null".to_owned(),
        _ => cbor_type_name(value).to_owned(),
    }
}

fn bounded_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// Live tool registration state keyed by connection and tool name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolRegistry {
    providers_by_tool: HashMap<ToolName, Vec<ToolProvider>>,
    tools_by_connection: HashMap<ConnectionId, Vec<ToolName>>,
}

impl ToolRegistry {
    /// Creates an empty tool registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one tool for a live provider connection without a prompt
    /// fragment.
    pub fn register(&mut self, connection_id: &str, tool: ToolSpec) -> RegisterToolReport {
        self.register_provider(
            connection_id,
            ToolRegister {
                tool,
                tool_group: None,
                prompt_fragment: None,
            },
            ToolProviderKind::Extension,
        )
    }

    /// Registers one harness-owned tool without a prompt fragment.
    pub fn register_internal(&mut self, connection_id: &str, tool: ToolSpec) -> RegisterToolReport {
        self.register_provider(
            connection_id,
            ToolRegister {
                tool,
                tool_group: None,
                prompt_fragment: None,
            },
            ToolProviderKind::Internal,
        )
    }

    /// Registers one tool for a live provider connection, including any prompt
    /// fragment attached to the registration.
    pub fn register_with_prompt_fragment(
        &mut self,
        connection_id: &str,
        registration: ToolRegister,
    ) -> RegisterToolReport {
        self.register_provider(connection_id, registration, ToolProviderKind::Extension)
    }

    fn register_provider(
        &mut self,
        connection_id: &str,
        registration: ToolRegister,
        kind: ToolProviderKind,
    ) -> RegisterToolReport {
        let ToolRegister {
            tool,
            tool_group,
            prompt_fragment,
        } = registration;
        let mut report = RegisterToolReport::default();
        if let Err(error) = validate_tool_examples(&tool) {
            report.errors.push(error);
            return report;
        }
        let tool_name = tool.name.clone();
        let providers = self.providers_by_tool.entry(tool_name.clone()).or_default();

        let existing_provider_ids = providers
            .iter()
            .map(|provider| provider.connection_id.clone())
            .collect::<Vec<_>>();
        if !existing_provider_ids.is_empty() {
            report
                .warnings
                .push(ToolRegistryWarning::DuplicateRegistration {
                    tool_name: tool_name.clone(),
                    existing_provider_ids,
                });
        }

        if let Some(existing_provider) = providers
            .iter_mut()
            .find(|provider| provider.connection_id == connection_id)
        {
            existing_provider.tool = tool;
            existing_provider.tool_group = tool_group;
            existing_provider.kind = kind;
            existing_provider.prompt_fragment = prompt_fragment;
        } else {
            providers.push(ToolProvider {
                connection_id: connection_id.into(),
                kind,
                tool,
                tool_group,
                prompt_fragment,
            });
        }

        let connection_tools = self
            .tools_by_connection
            .entry(connection_id.into())
            .or_default();
        if !connection_tools.contains(&tool_name) {
            connection_tools.push(tool_name);
        }

        report
    }

    /// Unregisters one tool from one provider connection.
    pub fn unregister(&mut self, connection_id: &str, tool_name: &str) -> bool {
        let mut removed = false;

        if let Some(providers) = self.providers_by_tool.get_mut(tool_name) {
            let initial_len = providers.len();
            providers.retain(|provider| provider.connection_id != connection_id);
            removed = providers.len() != initial_len;
            if providers.is_empty() {
                self.providers_by_tool.remove(tool_name);
            }
        }

        if removed {
            self.remove_tool_from_connection(connection_id, tool_name);
        }

        removed
    }

    /// Unregisters all tools owned by one disconnected provider connection.
    pub fn unregister_connection(&mut self, connection_id: &str) -> Vec<ToolName> {
        let Some(tool_names) = self.tools_by_connection.remove(connection_id) else {
            return Vec::new();
        };

        for tool_name in &tool_names {
            if let Some(providers) = self.providers_by_tool.get_mut(tool_name) {
                providers.retain(|provider| provider.connection_id != connection_id);
                if providers.is_empty() {
                    self.providers_by_tool.remove(tool_name);
                }
            }
        }

        tool_names
    }

    /// Returns all currently live providers for a tool name.
    #[must_use]
    pub fn providers_for(&self, tool_name: &str) -> Vec<ToolProvider> {
        self.providers_by_tool
            .get(tool_name)
            .cloned()
            .unwrap_or_default()
    }

    /// Returns all unique tool names currently registered.
    #[must_use]
    pub fn all_tool_names(&self) -> Vec<&ToolName> {
        self.providers_by_tool.keys().collect()
    }

    /// Returns all unique tool specs, one per tool name (first provider wins).
    #[must_use]
    pub fn all_tools(&self) -> Vec<&ToolSpec> {
        self.all_tool_providers()
            .into_iter()
            .map(|provider| &provider.tool)
            .collect()
    }

    /// Returns all unique tool providers, one per tool name (first provider
    /// wins), sorted by tool name for deterministic prompt and tool assembly.
    #[must_use]
    pub fn all_tool_providers(&self) -> Vec<&ToolProvider> {
        let mut providers: Vec<_> = self
            .providers_by_tool
            .values()
            .filter_map(|providers| providers.first())
            .collect();
        providers.sort_by(|a, b| a.tool.name.as_str().cmp(b.tool.name.as_str()));
        providers
    }

    /// Picks one currently live provider for a tool name.
    #[must_use]
    pub fn resolve_provider(&self, tool_name: &str) -> Option<&ToolProvider> {
        self.providers_by_tool
            .get(tool_name)
            .and_then(|providers| providers.first())
    }

    /// Resolves a `tool.request` to one live provider and builds the
    /// corresponding `tool.started` event.
    ///
    /// Success means the request is accepted and the harness can publish the
    /// started event. Failure means no provider was invoked; the harness
    /// reports that as a rejection event.
    pub fn route_tool_request(
        &self,
        request: ToolRequest,
    ) -> Result<ToolRouteReport, ToolRouteError> {
        let tool_name = request.tool_name.clone();
        let provider = self.resolve_provider(tool_name.as_str()).ok_or_else(|| {
            ToolRouteError::NoProvider {
                tool_name: tool_name.clone(),
            }
        })?;
        let target = match provider.kind {
            ToolProviderKind::Internal => ToolRouteTarget::Internal,
            ToolProviderKind::Extension => {
                ToolRouteTarget::Extension(provider.connection_id.clone())
            }
        };

        Ok(ToolRouteReport {
            target,
            invoke: ToolStarted {
                call_id: request.call_id,
                tool_name,
                arguments: request.arguments,
                agent_id: request.agent_id,
                originator: request.originator,
            },
        })
    }

    fn remove_tool_from_connection(&mut self, connection_id: &str, tool_name: &str) {
        if let Some(tool_names) = self.tools_by_connection.get_mut(connection_id) {
            tool_names.retain(|name| name != tool_name);
            if tool_names.is_empty() {
                self.tools_by_connection.remove(connection_id);
            }
        }
    }
}

#[cfg(test)]
mod tests;
