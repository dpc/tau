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
        examples: Vec::new(),
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

#[test]
fn repair_parses_json_object_string_when_schema_demands_object() {
    let tool = strict_tool(serde_json::json!({"type": "object"}));
    let repair = repair_tool_arguments(
        &tool,
        &CborValue::Text("{\"path\":\"src/lib.rs\"}".to_owned()),
    )
    .expect("repair");

    assert_eq!(
        repair.arguments,
        CborValue::Map(vec![(
            CborValue::Text("path".to_owned()),
            CborValue::Text("src/lib.rs".to_owned())
        )])
    );
    assert_eq!(
        repair.steps[0].kind,
        ToolArgumentRepairKind::JsonObjectStringToObject
    );
    validate_tool_arguments(&tool, &repair.arguments).expect("repaired args validate");
}

#[test]
fn repair_parses_json_array_string_when_schema_demands_array() {
    let tool = strict_tool(serde_json::json!({"type": "array"}));
    let repair =
        repair_tool_arguments(&tool, &CborValue::Text("[1,2]".to_owned())).expect("repair");

    assert_eq!(
        repair.arguments,
        CborValue::Array(vec![
            CborValue::Integer(1.into()),
            CborValue::Integer(2.into())
        ])
    );
    validate_tool_arguments(&tool, &repair.arguments).expect("repaired args validate");
}

#[test]
fn repair_removes_null_optional_fields() {
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "required": { "type": "string" },
            "optional": { "type": "string" }
        },
        "required": ["required"],
        "additionalProperties": false
    }));
    let repair = repair_tool_arguments(
        &tool,
        &CborValue::Map(vec![
            (
                CborValue::Text("required".to_owned()),
                CborValue::Text("value".to_owned()),
            ),
            (CborValue::Text("optional".to_owned()), CborValue::Null),
        ]),
    )
    .expect("repair");

    assert_eq!(
        repair.arguments,
        CborValue::Map(vec![(
            CborValue::Text("required".to_owned()),
            CborValue::Text("value".to_owned()),
        )])
    );
    validate_tool_arguments(&tool, &repair.arguments).expect("repaired args validate");
}

#[test]
fn repair_wraps_scalar_when_schema_demands_array() {
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": { "items": { "type": "array", "items": { "type": "integer" } } },
        "required": ["items"],
        "additionalProperties": false
    }));
    let repair = repair_tool_arguments(
        &tool,
        &CborValue::Map(vec![(
            CborValue::Text("items".to_owned()),
            CborValue::Integer(7.into()),
        )]),
    )
    .expect("repair");

    assert_eq!(
        repair.arguments,
        CborValue::Map(vec![(
            CborValue::Text("items".to_owned()),
            CborValue::Array(vec![CborValue::Integer(7.into())]),
        )])
    );
    validate_tool_arguments(&tool, &repair.arguments).expect("repaired args validate");
}

#[test]
fn repair_parses_integer_and_boolean_strings_when_schema_demands_them() {
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "count": { "type": "integer" },
            "enabled": { "type": "boolean" }
        },
        "required": ["count", "enabled"],
        "additionalProperties": false
    }));
    let repair = repair_tool_arguments(
        &tool,
        &CborValue::Map(vec![
            (
                CborValue::Text("count".to_owned()),
                CborValue::Text("-42".to_owned()),
            ),
            (
                CborValue::Text("enabled".to_owned()),
                CborValue::Text("false".to_owned()),
            ),
        ]),
    )
    .expect("repair");

    assert_eq!(
        repair.arguments,
        CborValue::Map(vec![
            (
                CborValue::Text("count".to_owned()),
                CborValue::Integer((-42).into()),
            ),
            (
                CborValue::Text("enabled".to_owned()),
                CborValue::Bool(false),
            ),
        ])
    );
    validate_tool_arguments(&tool, &repair.arguments).expect("repaired args validate");
}

#[test]
fn repair_does_not_rewrite_valid_or_ambiguous_arguments() {
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "count": { "type": "integer" },
            "required": { "type": "string" }
        },
        "required": ["name", "count", "required"],
        "additionalProperties": false
    }));
    let valid = CborValue::Map(vec![
        (
            CborValue::Text("name".to_owned()),
            CborValue::Text("7".to_owned()),
        ),
        (
            CborValue::Text("count".to_owned()),
            CborValue::Integer(7.into()),
        ),
        (
            CborValue::Text("required".to_owned()),
            CborValue::Text("present".to_owned()),
        ),
    ]);
    validate_tool_arguments(&tool, &valid).expect("valid");
    assert_eq!(repair_tool_arguments(&tool, &valid), None);

    let ambiguous = CborValue::Map(vec![
        (
            CborValue::Text("name".to_owned()),
            CborValue::Text("7".to_owned()),
        ),
        (
            CborValue::Text("count".to_owned()),
            CborValue::Text("4.2".to_owned()),
        ),
        (CborValue::Text("required".to_owned()), CborValue::Null),
    ]);
    assert_eq!(repair_tool_arguments(&tool, &ambiguous), None);

    let union_integer = strict_tool(serde_json::json!({"type": ["integer", "boolean"]}));
    assert_eq!(
        repair_tool_arguments(&union_integer, &CborValue::Text("7".to_owned())),
        None
    );
    let boolean_tool = strict_tool(serde_json::json!({"type": "boolean"}));
    assert_eq!(
        repair_tool_arguments(&boolean_tool, &CborValue::Text("TRUE".to_owned())),
        None
    );
}

#[test]
fn repair_does_not_remove_valid_optional_null() {
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "maybe": { "type": ["string", "null"] },
            "count": { "type": "integer" }
        },
        "required": ["count"],
        "additionalProperties": false
    }));
    let arguments = CborValue::Map(vec![
        (CborValue::Text("maybe".to_owned()), CborValue::Null),
        (
            CborValue::Text("count".to_owned()),
            CborValue::Text("7".to_owned()),
        ),
    ]);
    let repair = repair_tool_arguments(&tool, &arguments).expect("count repair");

    assert_eq!(
        repair.arguments,
        CborValue::Map(vec![
            (CborValue::Text("maybe".to_owned()), CborValue::Null),
            (
                CborValue::Text("count".to_owned()),
                CborValue::Integer(7.into()),
            ),
        ])
    );
}

#[test]
fn repair_rejects_duplicate_keys_in_json_object_strings() {
    let tool = strict_tool(serde_json::json!({"type": "object"}));

    assert_eq!(
        repair_tool_arguments(
            &tool,
            &CborValue::Text("{\"path\":\"safe\",\"path\":\"danger\"}".to_owned()),
        ),
        None
    );
}

#[test]
fn repair_rejects_nested_duplicate_keys_in_json_array_strings() {
    let tool = strict_tool(serde_json::json!({"type": "array"}));

    assert_eq!(
        repair_tool_arguments(
            &tool,
            &CborValue::Text("[{\"path\":\"safe\",\"path\":\"danger\"}]".to_owned()),
        ),
        None
    );
}

#[test]
fn repair_trace_is_bounded() {
    let properties = (0..(MAX_REPAIR_TRACE_STEPS + 4))
        .map(|idx| {
            (
                format!("field{idx}"),
                serde_json::json!({"type": "integer"}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let required = (0..(MAX_REPAIR_TRACE_STEPS + 4))
        .map(|idx| serde_json::Value::String(format!("field{idx}")))
        .collect::<Vec<_>>();
    let arguments = CborValue::Map(
        (0..(MAX_REPAIR_TRACE_STEPS + 4))
            .map(|idx| {
                (
                    CborValue::Text(format!("field{idx}")),
                    CborValue::Text(idx.to_string()),
                )
            })
            .collect(),
    );
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    }));
    let repair = repair_tool_arguments(&tool, &arguments).expect("repair");

    assert_eq!(repair.steps.len(), MAX_REPAIR_TRACE_STEPS);
    assert_eq!(repair.omitted_steps, 4);
    assert!(repair.render_summary().contains("… and 4 more"));
    assert!(repair.render_summary().len() <= MAX_DIAGNOSTIC_MESSAGE_CHARS);
}

#[test]
fn invalid_tool_example_rejects_registration() {
    let mut tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"],
        "additionalProperties": false
    }));
    tool.examples.push(ToolExample {
        id: "bad".to_owned(),
        title: None,
        arguments: CborValue::Map(vec![(
            CborValue::Text("path".to_owned()),
            CborValue::Integer(1.into()),
        )]),
        note: None,
        subcommand: None,
    });

    let mut registry = ToolRegistry::new();
    let report = registry.register("ext", tool);

    assert!(registry.providers_for("strict").is_empty());
    assert_eq!(report.errors.len(), 1);
    assert!(
        report.errors[0]
            .to_string()
            .contains("$.path: expected string")
    );
}

#[test]
fn oversized_tool_example_rejects_registration() {
    let mut tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"],
        "additionalProperties": false
    }));
    tool.examples.push(ToolExample {
        id: "huge".to_owned(),
        title: None,
        arguments: CborValue::Map(vec![(
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".repeat(MAX_TOOL_EXAMPLE_ARGUMENT_CHARS + 1)),
        )]),
        note: None,
        subcommand: None,
    });

    let error = validate_tool_examples(&tool).expect_err("oversized example must fail");

    assert!(
        error
            .to_string()
            .contains("arguments are too large for a compact example")
    );
}

#[test]
fn tool_example_hint_selects_matching_subcommand_deterministically() {
    let mut tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "operation": { "type": "string", "enum": ["insert", "replace"] },
            "path": { "type": "string" }
        },
        "required": ["operation", "path"],
        "additionalProperties": false
    }));
    tool.examples = vec![
        ToolExample {
            id: "replace".to_owned(),
            title: Some("Replace".to_owned()),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("operation".to_owned()),
                    CborValue::Text("replace".to_owned()),
                ),
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("src/lib.rs".to_owned()),
                ),
            ]),
            note: None,
            subcommand: Some(ToolExampleSelector {
                path: vec!["operation".to_owned()],
                value: CborValue::Text("replace".to_owned()),
            }),
        },
        ToolExample {
            id: "insert".to_owned(),
            title: Some("Insert".to_owned()),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("operation".to_owned()),
                    CborValue::Text("insert".to_owned()),
                ),
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("src/lib.rs".to_owned()),
                ),
            ]),
            note: None,
            subcommand: Some(ToolExampleSelector {
                path: vec!["operation".to_owned()],
                value: CborValue::Text("insert".to_owned()),
            }),
        },
    ];

    validate_tool_examples(&tool).expect("examples should be valid");
    let hint = tool_example_hint(
        &tool,
        &CborValue::Map(vec![(
            CborValue::Text("operation".to_owned()),
            CborValue::Text("replace".to_owned()),
        )]),
    )
    .expect("hint");

    assert!(hint.contains("Replace"));
    assert!(hint.contains("\"operation\":\"replace\""));
    assert!(!hint.contains("\"operation\":\"insert\""));
}

#[test]
fn tool_example_hint_falls_back_and_lists_subcommand_values() {
    let mut tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "operation": { "type": "string", "enum": ["insert"] },
            "path": { "type": "string" }
        },
        "required": ["operation", "path"],
        "additionalProperties": false
    }));
    tool.examples = vec![
        ToolExample {
            id: "generic".to_owned(),
            title: Some("Generic".to_owned()),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("operation".to_owned()),
                    CborValue::Text("insert".to_owned()),
                ),
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("src/lib.rs".to_owned()),
                ),
            ]),
            note: None,
            subcommand: None,
        },
        ToolExample {
            id: "insert".to_owned(),
            title: Some("Insert".to_owned()),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("operation".to_owned()),
                    CborValue::Text("insert".to_owned()),
                ),
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("src/lib.rs".to_owned()),
                ),
            ]),
            note: None,
            subcommand: Some(ToolExampleSelector {
                path: vec!["operation".to_owned()],
                value: CborValue::Text("insert".to_owned()),
            }),
        },
    ];

    let hint = tool_example_hint(&tool, &CborValue::Map(Vec::new())).expect("hint");

    assert!(hint.contains("Generic"));
    assert!(hint.contains("Subcommand values include: `insert`"));
    assert!(hint.chars().count() <= MAX_TOOL_EXAMPLE_HINT_CHARS);

    let invalid_value_hint = tool_example_hint(
        &tool,
        &CborValue::Map(vec![(
            CborValue::Text("operation".to_owned()),
            CborValue::Text("delete".to_owned()),
        )]),
    )
    .expect("hint for invalid selector value");
    assert!(invalid_value_hint.contains("Subcommand values include: `insert`"));
}

#[test]
fn tool_example_validation_reports_selector_path_errors() {
    let mut tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "operation": { "type": "string", "enum": ["insert"] },
            "path": { "type": "string" }
        },
        "required": ["operation", "path"],
        "additionalProperties": false
    }));
    tool.examples.push(ToolExample {
        id: "missing-selector-path".to_owned(),
        title: None,
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("operation".to_owned()),
                CborValue::Text("insert".to_owned()),
            ),
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text("src/lib.rs".to_owned()),
            ),
        ]),
        note: None,
        subcommand: Some(ToolExampleSelector {
            path: vec!["command".to_owned()],
            value: CborValue::Text("insert".to_owned()),
        }),
    });

    let error = validate_tool_examples(&tool).expect_err("selector path must fail");

    assert!(
        error
            .to_string()
            .contains("subcommand selector path is absent")
    );
}

#[test]
fn tool_example_validation_reports_selector_value_errors() {
    let mut tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "operation": { "type": "string", "enum": ["insert", "replace"] },
            "path": { "type": "string" }
        },
        "required": ["operation", "path"],
        "additionalProperties": false
    }));
    tool.examples.push(ToolExample {
        id: "mismatched-selector-value".to_owned(),
        title: None,
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("operation".to_owned()),
                CborValue::Text("insert".to_owned()),
            ),
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text("src/lib.rs".to_owned()),
            ),
        ]),
        note: None,
        subcommand: Some(ToolExampleSelector {
            path: vec!["operation".to_owned()],
            value: CborValue::Text("replace".to_owned()),
        }),
    });

    let error = validate_tool_examples(&tool).expect_err("selector value must fail");

    assert!(
        error
            .to_string()
            .contains("subcommand selector value does not match")
    );
}
