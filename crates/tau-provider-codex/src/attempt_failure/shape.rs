use std::cmp::Ordering;

use serde_json::{Map, Value};

const MAX_SHAPE_DEPTH: usize = 16;
const MAX_SHAPE_NODES: usize = 1_024;
const MAX_CONTAINER_ENTRIES: usize = 128;

/// Result of bounded structural projection.
pub(super) struct ShapeProjection {
    /// Value-free projected shape, absent when the root exceeded a bound.
    pub(super) value: Option<Value>,
    /// Whether any depth, node, entry, key, or collision bound applied.
    pub(super) truncated: bool,
}

/// Replace raw values and keys with the approved bounded structural shape.
pub(super) fn project_shape(value: &Value) -> ShapeProjection {
    let mut budget = ShapeBudget::default();
    let value = project_shape_inner(value, 0, &mut budget);
    ShapeProjection {
        value,
        truncated: budget.truncated,
    }
}

#[derive(Default)]
/// Mutable global budget shared by one recursive shape projection.
struct ShapeBudget {
    /// Number of emitted scalar and container nodes.
    nodes: usize,
    /// Whether projection omitted or renamed any provider-controlled structure.
    truncated: bool,
}

/// One map field retained by the fixed-cap lexical selector.
struct RetainedField<'a> {
    /// Provider key borrowed only for bounded comparison and projection.
    key: &'a str,
    /// Provider value projected after selection.
    value: &'a Value,
    /// Whether both the scalar and UTF-8 key bounds admit lexical comparison.
    key_within_bound: bool,
}

impl RetainedField<'_> {
    fn compare(&self, other: &Self) -> Ordering {
        match (self.key_within_bound, other.key_within_bound) {
            (true, true) => self.key.cmp(other.key),
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => Ordering::Equal,
        }
    }
}

fn project_shape_inner(value: &Value, depth: usize, budget: &mut ShapeBudget) -> Option<Value> {
    if MAX_SHAPE_NODES <= budget.nodes
        || (MAX_SHAPE_DEPTH <= depth && matches!(value, Value::Array(_) | Value::Object(_)))
    {
        budget.truncated = true;
        return None;
    }
    budget.nodes += 1;
    Some(match value {
        Value::Null => Value::String("null".to_owned()),
        Value::Bool(_) => Value::String("boolean".to_owned()),
        Value::Number(_) => Value::String("number".to_owned()),
        Value::String(_) => Value::String("string".to_owned()),
        Value::Array(values) => {
            if MAX_CONTAINER_ENTRIES < values.len() {
                budget.truncated = true;
            }
            Value::Array(
                values
                    .iter()
                    .take(MAX_CONTAINER_ENTRIES)
                    .filter_map(|value| project_shape_inner(value, depth + 1, budget))
                    .collect(),
            )
        }
        Value::Object(fields) => {
            if MAX_CONTAINER_ENTRIES < fields.len() {
                budget.truncated = true;
            }
            let mut retained = Vec::with_capacity(fields.len().min(MAX_CONTAINER_ENTRIES));
            for (key, value) in fields {
                let candidate = RetainedField {
                    key,
                    value,
                    key_within_bound: key.len() <= 512 && key.chars().take(129).count() <= 128,
                };
                if retained.len() < MAX_CONTAINER_ENTRIES {
                    retained.push(candidate);
                    continue;
                }
                let (largest_index, largest) = retained
                    .iter()
                    .enumerate()
                    .max_by(|(_, left), (_, right)| left.compare(right))
                    .expect("fixed-cap retained fields are nonempty");
                if candidate.compare(largest).is_lt() {
                    retained[largest_index] = candidate;
                }
            }
            retained.sort_unstable_by(RetainedField::compare);
            let mut output = Map::new();
            for (
                position,
                RetainedField {
                    key,
                    value,
                    key_within_bound,
                },
            ) in retained.into_iter().enumerate()
            {
                let sensitive = key_within_bound && is_sensitive_key(key);
                if !key_within_bound {
                    budget.truncated = true;
                }
                let mut key = if sensitive {
                    "<redacted-key>".to_owned()
                } else if key_within_bound && allowed_shape_key(key) {
                    key.to_owned()
                } else {
                    format!("<field-{position}>")
                };
                if output.contains_key(&key) {
                    budget.truncated = true;
                    key = format!("<field-{position}>");
                }
                let value = if sensitive || !key_within_bound {
                    if MAX_SHAPE_NODES <= budget.nodes {
                        budget.truncated = true;
                        break;
                    }
                    budget.nodes += 1;
                    Value::String("redacted".to_owned())
                } else if let Some(value) = project_shape_inner(value, depth + 1, budget) {
                    value
                } else {
                    break;
                };
                output.insert(key, value);
            }
            Value::Object(output)
        }
    })
}

fn allowed_shape_key(key: &str) -> bool {
    matches!(
        key,
        "type"
            | "response"
            | "error"
            | "id"
            | "request_id"
            | "response_id"
            | "code"
            | "message"
            | "status"
            | "incomplete_details"
            | "reason"
            | "resets_in_seconds"
            | "resets_at"
            | "usage"
            | "input_tokens"
            | "output_tokens"
            | "total_tokens"
    )
}

fn is_sensitive_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "authorization"
            | "api_key"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "secret"
            | "password"
            | "cookie"
            | "set-cookie"
    )
}
