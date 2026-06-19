use super::*;

fn strict_tool(parameters: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: ToolName::new("strict"),
        model_visible_name: None,
        description: Some("strict test tool".to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(parameters),
        format: None,
        enabled_by_default: true,
        tags: Vec::new(),
        background_support: None,
    }
}

/// Ensures type mismatches identify the exact schema path, expected type,
/// and actual supplied type so a model can fix the argument mechanically.
#[test]
fn validation_error_reports_path_expected_type_and_actual_type() {
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": { "count": { "type": "integer" } },
        "required": ["count"],
        "additionalProperties": false
    }));
    let error = validate_tool_arguments(
        &tool,
        &CborValue::Map(vec![(
            CborValue::Text("count".to_owned()),
            CborValue::Text("one".to_owned()),
        )]),
    )
    .expect_err("string must fail integer schema");

    assert_eq!(error.to_string(), "$.count: expected integer, got string");
}

/// Ensures enum failures list allowed values and include a near-value hint
/// only when the edit-distance match is clear.
#[test]
fn validation_error_reports_enum_values_and_nearest_match() {
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "mode": { "type": "string", "enum": ["replace", "insert"] }
        },
        "required": ["mode"],
        "additionalProperties": false
    }));
    let error = validate_tool_arguments(
        &tool,
        &CborValue::Map(vec![(
            CborValue::Text("mode".to_owned()),
            CborValue::Text("replce".to_owned()),
        )]),
    )
    .expect_err("misspelled enum must fail");

    let message = error.to_string();
    assert!(message.contains("$.mode: invalid enum value `replce`"));
    assert!(message.contains("allowed values: `replace`, `insert`"));
    assert!(message.contains("did you mean `replace`?"));
}

/// Ensures enum diagnostics are bounded even when the rejected value and
/// allowed values are long or numerous, preventing provider-controlled
/// schemas from generating unbounded model-visible errors.
#[test]
fn validation_error_bounds_long_and_many_enum_values() {
    let long_value = "x".repeat(200);
    let values = (0..40)
        .map(|idx| serde_json::Value::String(format!("value-{idx}-{}", "y".repeat(100))))
        .collect::<Vec<_>>();
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "mode": { "type": "string", "enum": values }
        },
        "required": ["mode"],
        "additionalProperties": false
    }));
    let error = validate_tool_arguments(
        &tool,
        &CborValue::Map(vec![(
            CborValue::Text("mode".to_owned()),
            CborValue::Text(long_value.clone()),
        )]),
    )
    .expect_err("unknown enum value must fail");

    let message = error.to_string();
    assert!(
        message.len() <= MAX_DIAGNOSTIC_MESSAGE_CHARS + "$.mode: ".len(),
        "message too long: {}",
        message.len()
    );
    assert!(!message.contains(&long_value));
    assert!(message.contains(&format!("{}…", "x".repeat(MAX_DIAGNOSTIC_ITEM_CHARS))));
    assert!(message.contains("… and more"));
}

/// Ensures additionalProperties validation cannot echo an unbounded object key
/// through the rendered schema path when a nested dynamic property fails.
#[test]
fn validation_error_bounds_dynamic_property_path_segments() {
    let long_key = "k".repeat(400);
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "additionalProperties": { "type": "integer" }
    }));
    let error = validate_tool_arguments(
        &tool,
        &CborValue::Map(vec![(
            CborValue::Text(long_key.clone()),
            CborValue::Text("not an integer".to_owned()),
        )]),
    )
    .expect_err("wrong additional property type must fail");

    let message = error.to_string();
    assert!(message.len() < MAX_DIAGNOSTIC_PATH_CHARS + MAX_DIAGNOSTIC_MESSAGE_CHARS);
    assert!(!message.contains(&long_key));
    assert!(message.contains("$.kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk…"));
}

/// Ensures all missing required fields are reported together rather than
/// forcing a model through one reject/retry cycle per missing property.
#[test]
fn validation_error_reports_all_missing_required_fields() {
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "edits": { "type": "array" }
        },
        "required": ["path", "edits"],
        "additionalProperties": false
    }));
    let error = validate_tool_arguments(&tool, &CborValue::Map(Vec::new()))
        .expect_err("missing fields must fail");

    assert_eq!(
        error.to_string(),
        "missing required argument(s): `path`, `edits`"
    );
}

/// Ensures closed-object failures list all unknown fields and the complete
/// allowed field set so the model knows what to remove and what keys
/// remain.
#[test]
fn validation_error_reports_unknown_and_allowed_fields() {
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "edits": { "type": "array" }
        },
        "required": ["path"],
        "additionalProperties": false
    }));
    let error = validate_tool_arguments(
        &tool,
        &CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text("src/lib.rs".to_owned()),
            ),
            (CborValue::Text("foo".to_owned()), CborValue::Bool(true)),
            (CborValue::Text("bar".to_owned()), CborValue::Bool(true)),
        ]),
    )
    .expect_err("unknown fields must fail");

    assert_eq!(
        error.to_string(),
        "unexpected argument(s): `foo`, `bar`; allowed fields: `edits`, `path`"
    );
}
