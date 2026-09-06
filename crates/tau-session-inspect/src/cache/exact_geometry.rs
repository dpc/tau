//! Content-free exact-request evidence derived offline from private captures.

use std::collections::{BTreeMap, BTreeSet};

use blake3::Hasher;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[cfg(test)]
mod tests;

/// One inspection-local secret used to make captured values
/// equality-comparable.
#[derive(Clone)]
pub(super) struct FingerprintKey(pub(super) [u8; 32]);

impl FingerprintKey {
    /// Generates a fresh key from the operating system random source.
    pub(super) fn random() -> Result<Self, &'static str> {
        use rand::RngCore as _;
        let mut key = [0; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut key)
            .map_err(|_| "cache_index_random_key_unavailable")?;
        Ok(Self(key))
    }
}

/// Fixed-size private request evidence; no captured body or provider identifier
/// remains.
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct ExactRequest {
    /// Typed durable session attribution.
    pub(super) session: String,
    /// Typed provider-owner attribution.
    pub(super) agent: String,
    /// Typed prompt attribution.
    pub(super) prompt: String,
    /// Keyed provider-instance identity.
    pub(super) instance: String,
    /// Keyed attempt identity, absent for unsupported legacy captures.
    pub(super) attempt: Option<String>,
    /// Actual capture dispatch index; public Responses requests deliberately
    /// lack it.
    pub(super) dispatch: Option<u64>,
    /// Closed request adapter.
    pub(super) adapter: String,
    /// Keyed complete captured-body structure.
    pub(super) body: String,
    /// Keyed instruction structure when separately represented.
    pub(super) instructions: Option<String>,
    /// Keyed ordered complete tool declarations.
    pub(super) tools: String,
    /// Keyed typed request controls.
    pub(super) controls: String,
    /// Keyed members not assigned to another closed category.
    pub(super) other: String,
    /// Keyed route identity from capture-local backend, transport, and model.
    pub(super) route: String,
    /// Keyed prompt-cache key, if present.
    pub(super) cache_key: Option<String>,
    /// Keyed previous-response identity, if present.
    pub(super) previous_response: Option<String>,
    /// Ordered keyed item structures.
    pub(super) items: Vec<String>,
    /// Cumulative keyed ordered-prefix structures.
    pub(super) prefixes: Vec<String>,
    /// Whether the capture contained a complete request body.
    pub(super) complete: bool,
    /// Whether this row came from an earlier disposable index.
    #[serde(default)]
    pub(super) indexed: bool,
    /// Scalar diagnostic observation time used only for stable comparison
    /// order.
    #[serde(default)]
    pub(super) recorded_at_unix_micros: Option<u64>,
    /// Closed scalar request form used to reject suffix-as-full comparisons.
    #[serde(default)]
    pub(super) request_form: Option<String>,
}

/// Fixed-size successful-response identity used only to qualify explicit
/// chains.
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct ExactResponse {
    /// Keyed provider-instance identity.
    pub(super) instance: String,
    /// Keyed attempt identity.
    pub(super) attempt: Option<String>,
    /// Actual dispatch index where captured.
    pub(super) dispatch: Option<u64>,
    /// Keyed provider response identity.
    pub(super) response: String,
    /// Whether this row came from an earlier disposable index.
    #[serde(default)]
    pub(super) indexed: bool,
}

/// Hashes one capture-local identity in the same domain used by stored
/// evidence.
pub(super) fn identity(key: &FingerprintKey, domain: &[u8], value: &str) -> String {
    fingerprint_bytes(key, domain, value.as_bytes())
}

/// Extracts one exact request without retaining its private body.
pub(super) fn request(
    key: &FingerprintKey,
    instance: &str,
    capture: &Value,
) -> Result<ExactRequest, &'static str> {
    let body = capture
        .get("body")
        .and_then(Value::as_object)
        .ok_or("exact_request_body_unavailable")?;
    let backend = capture
        .get("backend")
        .and_then(Value::as_str)
        .ok_or("exact_request_adapter_unavailable")?;
    let (adapter, input_name, control_names): (&str, &str, &[&str]) = match backend {
        "responses" => (
            "responses",
            "input",
            &[
                "model",
                "include",
                "parallel_tool_calls",
                "reasoning",
                "service_tier",
                "store",
                "text",
                "tool_choice",
                "max_output_tokens",
                "prompt_cache_options",
                "type",
            ],
        ),
        "chat_completions" => (
            "chat_completions",
            "messages",
            &[
                "model",
                "stream",
                "stream_options",
                "tool_choice",
                "parallel_tool_calls",
                "reasoning_effort",
                "max_tokens",
                "max_completion_tokens",
                "prompt_cache_options",
            ],
        ),
        _ => return Err("exact_request_adapter_unavailable"),
    };
    let input = body.get(input_name).and_then(Value::as_array);
    let complete = input.is_some();
    let items = input
        .into_iter()
        .flatten()
        .map(|item| fingerprint(key, b"input-item", item))
        .collect::<Vec<_>>();
    let mut prefixes = Vec::with_capacity(items.len());
    let mut prefix = Vec::new();
    for item in &items {
        prefix.push(Value::String(item.clone()));
        prefixes.push(fingerprint(
            key,
            b"input-prefix",
            &Value::Array(prefix.clone()),
        ));
    }
    let tools = body
        .get("tools")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let instructions = body
        .get("instructions")
        .map(|value| fingerprint(key, b"instructions", value));
    let controls = selected_object(body, control_names);
    let excluded = control_names
        .iter()
        .copied()
        .chain([
            input_name,
            "instructions",
            "tools",
            "previous_response_id",
            "prompt_cache_key",
        ])
        .collect::<BTreeSet<_>>();
    let other = body
        .iter()
        .filter(|(name, _)| !excluded.contains(name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let route = serde_json::json!({
        "backend": backend,
        "transport": capture.get("transport"),
        "model": capture.get("model"),
    });
    Ok(ExactRequest {
        session: capture["session_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        agent: capture["agent_id"].as_str().unwrap_or_default().to_owned(),
        prompt: capture["agent_prompt_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        instance: fingerprint_bytes(key, b"provider-instance", instance.as_bytes()),
        attempt: capture
            .get("attempt_id")
            .and_then(Value::as_str)
            .map(|id| fingerprint_bytes(key, b"attempt-id", id.as_bytes())),
        dispatch: capture.get("wire_dispatch_index").and_then(Value::as_u64),
        adapter: adapter.to_owned(),
        body: fingerprint(key, b"canonical-body", &Value::Object(body.clone())),
        instructions,
        tools: fingerprint(key, b"tools", &tools),
        controls: fingerprint(key, b"controls", &Value::Object(controls)),
        other: fingerprint(key, b"other-fields", &Value::Object(other)),
        route: fingerprint(key, b"route", &route),
        cache_key: body
            .get("prompt_cache_key")
            .map(|value| fingerprint(key, b"cache-key", value)),
        previous_response: body
            .get("previous_response_id")
            .map(|value| fingerprint(key, b"response-id", value)),
        items,
        prefixes,
        complete,
        indexed: false,
        recorded_at_unix_micros: None,
        request_form: None,
    })
}

/// Extracts one successful response ID for explicit chain qualification.
pub(super) fn response(
    key: &FingerprintKey,
    instance: &str,
    capture: &Value,
) -> Option<ExactResponse> {
    let response = capture.get("provider_response_id")?.as_str()?;
    Some(ExactResponse {
        instance: fingerprint_bytes(key, b"provider-instance", instance.as_bytes()),
        attempt: capture
            .get("attempt_id")
            .and_then(Value::as_str)
            .map(|id| fingerprint_bytes(key, b"attempt-id", id.as_bytes())),
        dispatch: capture.get("wire_dispatch_index").and_then(Value::as_u64),
        response: fingerprint_bytes(key, b"response-id", response.as_bytes()),
        indexed: false,
    })
}

/// Returns an equality classification without treating absence as equality.
pub(super) fn equality(left: Option<&String>, right: Option<&String>) -> &'static str {
    match (left, right) {
        (Some(left), Some(right)) if left == right => "equal",
        (Some(_), Some(_)) => "different",
        _ => "unknown",
    }
}

/// Counts the equal ordered item prefix shared by two complete request bodies.
pub(super) fn common_prefix(left: &ExactRequest, right: &ExactRequest) -> Option<usize> {
    (left.complete && right.complete).then(|| {
        left.items
            .iter()
            .zip(&right.items)
            .take_while(|(left, right)| left == right)
            .count()
    })
}

/// Builds a canonical object containing only named fields that are present.
fn selected_object(body: &Map<String, Value>, names: &[&str]) -> Map<String, Value> {
    names
        .iter()
        .filter_map(|name| {
            body.get(*name)
                .map(|value| ((*name).to_owned(), value.clone()))
        })
        .collect()
}

/// Produces a domain-separated keyed canonical JSON fingerprint.
fn fingerprint(key: &FingerprintKey, domain: &[u8], value: &Value) -> String {
    let mut hasher = Hasher::new_keyed(&key.0);
    update(&mut hasher, b"tau.cache.geometry.v0");
    update(&mut hasher, domain);
    canonical(&mut hasher, value);
    encode(hasher.finalize().as_bytes())
}

/// Produces a domain-separated keyed byte fingerprint.
fn fingerprint_bytes(key: &FingerprintKey, domain: &[u8], value: &[u8]) -> String {
    let mut hasher = Hasher::new_keyed(&key.0);
    update(&mut hasher, b"tau.cache.geometry.v0");
    update(&mut hasher, domain);
    update(&mut hasher, value);
    encode(hasher.finalize().as_bytes())
}

/// Hashes JSON by scalar type, exact value, array order, and sorted object
/// keys.
fn canonical(hasher: &mut Hasher, value: &Value) {
    match value {
        Value::Null => update(hasher, b"null"),
        Value::Bool(value) => update(hasher, if *value { b"true" } else { b"false" }),
        Value::Number(value) => {
            update(hasher, b"number");
            update(hasher, value.to_string().as_bytes());
        }
        Value::String(value) => {
            update(hasher, b"string");
            update(hasher, value.as_bytes());
        }
        Value::Array(values) => {
            update(hasher, b"array");
            update(hasher, &(values.len() as u64).to_le_bytes());
            for value in values {
                canonical(hasher, value);
            }
        }
        Value::Object(values) => {
            update(hasher, b"object");
            update(hasher, &(values.len() as u64).to_le_bytes());
            let sorted = values.iter().collect::<BTreeMap<_, _>>();
            for (name, value) in sorted {
                update(hasher, name.as_bytes());
                canonical(hasher, value);
            }
        }
    }
}

/// Adds one unambiguous length-delimited component.
fn update(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Encodes a digest without another dependency.
fn encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}
