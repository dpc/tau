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

/// Runs the production validator while retaining its exact semantic object
/// indexing and lookup work for complexity assertions.
fn validate_with_object_work(
    tool: &ToolSpec,
    arguments: &CborValue,
) -> (
    Result<(), ToolArgumentValidationError>,
    ObjectValidationWork,
) {
    let mut work = ObjectValidationWork::default();
    let result = validate_json_schema_with_work(
        tool.parameters.as_ref().expect("strict tool schema"),
        arguments,
        "$",
        &mut work,
    );
    (result, work)
}

/// Reproduces the previous object-member rescans as a differential oracle,
/// while delegating unchanged non-object schema semantics to production.
fn legacy_validate_json_schema(
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
        CborValue::Map(entries) => legacy_validate_object_schema(schema, entries, path),
        _ => validate_json_schema(&serde_json::Value::Object(schema.clone()), value, path),
    }
}

/// Implements the exact pre-index object algorithm retained as a test oracle.
fn legacy_validate_object_schema(
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
                    .any(|(key, _)| matches!(key, CborValue::Text(key) if key == *required_name))
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
            legacy_validate_json_schema(field_schema, field_value, &child_path(path, field_name))?;
            continue;
        }
        if let Some(additional_schema @ serde_json::Value::Object(_)) =
            schema.get("additionalProperties")
        {
            legacy_validate_json_schema(
                additional_schema,
                field_value,
                &child_path(path, field_name),
            )?;
        }
    }
    Ok(())
}

/// Builds a deterministic closed schema with every generated field required.
fn wide_required_schema(field_count: usize) -> serde_json::Value {
    let properties = (0..field_count)
        .map(|idx| {
            (
                format!("field-{idx:04}"),
                serde_json::json!({"type": "integer"}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let required = (0..field_count)
        .map(|idx| serde_json::Value::String(format!("field-{idx:04}")))
        .collect::<Vec<_>>();
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// Builds ordered valid arguments for [`wide_required_schema`].
fn wide_arguments(field_count: usize) -> Vec<(CborValue, CborValue)> {
    (0..field_count)
        .map(|idx| {
            (
                CborValue::Text(format!("field-{idx:04}")),
                CborValue::Integer((idx as i64).into()),
            )
        })
        .collect()
}

/// Converts validation outcomes into directly comparable wire diagnostics.
fn rendered_validation(result: Result<(), ToolArgumentValidationError>) -> Result<(), String> {
    result.map_err(|error| error.to_string())
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

/// Ensures numeric bound checks do not accept non-finite floats or let large
/// integers bypass integral JSON bounds through lossy `f64` conversion.
#[test]
fn validation_error_rejects_non_finite_and_imprecise_numbers() {
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "ratio": { "type": "number" },
            "count": { "type": "integer", "maximum": 9007199254740992_i64 }
        },
        "required": ["ratio", "count"],
        "additionalProperties": false
    }));

    let nan_error = validate_tool_arguments(
        &tool,
        &CborValue::Map(vec![
            (
                CborValue::Text("ratio".to_owned()),
                CborValue::Float(f64::NAN),
            ),
            (
                CborValue::Text("count".to_owned()),
                CborValue::Integer(1.into()),
            ),
        ]),
    )
    .expect_err("non-finite float must fail");
    assert_eq!(nan_error.to_string(), "$.ratio: number must be finite");

    let large_integer_error = validate_tool_arguments(
        &tool,
        &CborValue::Map(vec![
            (CborValue::Text("ratio".to_owned()), CborValue::Float(1.0)),
            (
                CborValue::Text("count".to_owned()),
                CborValue::Integer(9_007_199_254_740_993_i64.into()),
            ),
        ]),
    )
    .expect_err("integer beyond exact maximum must fail");
    assert_eq!(
        large_integer_error.to_string(),
        "$.count: must be at most 9007199254740992"
    );
}

/// Ensures unsigned integer bounds above `i64::MAX` are compared exactly
/// instead of falling through to lossy floating-point comparison.
#[test]
fn validation_error_checks_unsigned_integer_bounds_exactly() {
    let tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "count": {
                "type": "integer",
                "minimum": 9223372036854775808_u64,
                "maximum": 9223372036854775809_u64
            }
        },
        "required": ["count"],
        "additionalProperties": false
    }));

    let too_small = validate_tool_arguments(
        &tool,
        &CborValue::Map(vec![(
            CborValue::Text("count".to_owned()),
            CborValue::Integer(9_223_372_036_854_775_807_i64.into()),
        )]),
    )
    .expect_err("integer below unsigned minimum must fail");
    assert_eq!(
        too_small.to_string(),
        "$.count: must be at least 9223372036854775808"
    );

    let too_large = validate_tool_arguments(
        &tool,
        &CborValue::Map(vec![(
            CborValue::Text("count".to_owned()),
            CborValue::Integer(9_223_372_036_854_775_810_u64.into()),
        )]),
    )
    .expect_err("integer above unsigned maximum must fail");
    assert_eq!(
        too_large.to_string(),
        "$.count: must be at most 9223372036854775809"
    );
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

/// Proves the production object-key index stays differential-equivalent to the
/// previous rescanning algorithm while its counted work scales with field
/// count rather than required-field × argument-field count.
#[test]
fn wide_required_validation_is_linear_and_differentially_equivalent() {
    for field_count in [100, 500, 1_000, 2_000] {
        let schema = wide_required_schema(field_count);
        let tool = strict_tool(schema.clone());
        let valid_entries = wide_arguments(field_count);
        let valid_arguments = CborValue::Map(valid_entries.clone());

        let (actual, work) = validate_with_object_work(&tool, &valid_arguments);
        let legacy = legacy_validate_json_schema(&schema, &valid_arguments, "$");
        assert_eq!(rendered_validation(actual), rendered_validation(legacy));
        assert_eq!(work.indexed_entries, field_count);
        assert_eq!(work.required_lookups, field_count);
        assert_eq!(work.closed_key_visits, field_count);
        assert!(
            work.indexed_entries + work.required_lookups + work.closed_key_visits
                <= field_count * 3
        );

        let mut missing_entries = valid_entries.iter().skip(20).cloned().collect::<Vec<_>>();
        missing_entries.push((
            CborValue::Text("field-0020".to_owned()),
            CborValue::Integer(20.into()),
        ));
        let missing_arguments = CborValue::Map(missing_entries);
        let (actual, work) = validate_with_object_work(&tool, &missing_arguments);
        let legacy = legacy_validate_json_schema(&schema, &missing_arguments, "$");
        assert_eq!(rendered_validation(actual), rendered_validation(legacy));
        assert_eq!(work.indexed_entries, field_count - 19);
        assert_eq!(work.required_lookups, MAX_DIAGNOSTIC_ITEMS + 1);
        assert_eq!(work.closed_key_visits, 0);
        let error = validate_tool_arguments(&tool, &missing_arguments)
            .expect_err("first twenty authored required names are missing")
            .to_string();
        for idx in 0..MAX_DIAGNOSTIC_ITEMS {
            assert!(error.contains(&format!("`field-{idx:04}`")));
        }
        assert!(error.ends_with("… and more"));

        let mut unknown_entries = valid_entries.clone();
        unknown_entries.extend((0..20).map(|idx| {
            (
                CborValue::Text(format!("unknown-{idx:02}")),
                CborValue::Bool(true),
            )
        }));
        let unknown_arguments = CborValue::Map(unknown_entries);
        let (actual, work) = validate_with_object_work(&tool, &unknown_arguments);
        let legacy = legacy_validate_json_schema(&schema, &unknown_arguments, "$");
        assert_eq!(rendered_validation(actual), rendered_validation(legacy));
        assert_eq!(work.indexed_entries, field_count + 20);
        assert_eq!(work.required_lookups, field_count);
        assert_eq!(
            work.closed_key_visits,
            field_count + MAX_DIAGNOSTIC_ITEMS + 1
        );
        let error = validate_tool_arguments(&tool, &unknown_arguments)
            .expect_err("twenty unknown names exceed the diagnostic cap")
            .to_string();
        for idx in 0..MAX_DIAGNOSTIC_ITEMS {
            assert!(error.contains(&format!("`unknown-{idx:02}`")));
        }
        assert!(error.contains("… and more"));

        let mut duplicate_entries = valid_entries.clone();
        duplicate_entries.insert(
            1,
            (
                CborValue::Text("field-0000".to_owned()),
                CborValue::Text("invalid duplicate".to_owned()),
            ),
        );
        let duplicate_arguments = CborValue::Map(duplicate_entries);
        let (actual, work) = validate_with_object_work(&tool, &duplicate_arguments);
        let legacy = legacy_validate_json_schema(&schema, &duplicate_arguments, "$");
        let actual = rendered_validation(actual);
        assert_eq!(actual, rendered_validation(legacy));
        assert_eq!(
            actual,
            Err("$.field-0000: expected integer, got string".to_owned())
        );
        assert_eq!(work.indexed_entries, field_count + 1);
        assert_eq!(work.required_lookups, field_count);
        assert_eq!(work.closed_key_visits, field_count + 1);

        let mut non_string_entries = valid_entries;
        non_string_entries.push((CborValue::Integer(7.into()), CborValue::Null));
        let non_string_arguments = CborValue::Map(non_string_entries);
        let (actual, work) = validate_with_object_work(&tool, &non_string_arguments);
        let legacy = legacy_validate_json_schema(&schema, &non_string_arguments, "$");
        let actual = rendered_validation(actual);
        assert_eq!(actual, rendered_validation(legacy));
        assert_eq!(actual, Err("object keys must be strings".to_owned()));
        assert_eq!(work.indexed_entries, field_count + 1);
        assert_eq!(work.required_lookups, field_count);
        assert_eq!(work.closed_key_visits, field_count);
    }
}

/// Ensures nested wide objects reuse the same linear key-view path and that
/// optional-null repair still revalidates against the differential oracle.
#[test]
fn nested_wide_validation_and_repair_preserve_linear_work() {
    for field_count in [100, 500, 1_000, 2_000] {
        let inner_schema = wide_required_schema(field_count);
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"nested": inner_schema},
            "required": ["nested"],
            "additionalProperties": false
        });
        let tool = strict_tool(schema.clone());
        let arguments = CborValue::Map(vec![(
            CborValue::Text("nested".to_owned()),
            CborValue::Map(wide_arguments(field_count)),
        )]);
        let (actual, work) = validate_with_object_work(&tool, &arguments);
        let legacy = legacy_validate_json_schema(&schema, &arguments, "$");
        assert_eq!(rendered_validation(actual), rendered_validation(legacy));
        assert_eq!(work.indexed_entries, field_count + 1);
        assert_eq!(work.required_lookups, field_count + 1);
        assert_eq!(work.closed_key_visits, field_count + 1);

        let mut repair_schema = wide_required_schema(field_count);
        repair_schema["required"]
            .as_array_mut()
            .expect("required array")
            .pop();
        let repair_tool = strict_tool(repair_schema.clone());
        let mut repair_entries = wide_arguments(field_count);
        repair_entries[field_count - 1].1 = CborValue::Null;
        let repair = repair_tool_arguments(&repair_tool, &CborValue::Map(repair_entries))
            .expect("optional invalid null is removed");
        let (actual, repair_work) = validate_with_object_work(&repair_tool, &repair.arguments);
        let legacy = legacy_validate_json_schema(&repair_schema, &repair.arguments, "$");
        assert_eq!(rendered_validation(actual), rendered_validation(legacy));
        assert_eq!(
            repair.arguments,
            CborValue::Map(wide_arguments(field_count - 1))
        );
        assert_eq!(repair_work.indexed_entries, field_count - 1);
        assert_eq!(repair_work.required_lookups, field_count - 1);
        assert_eq!(repair_work.closed_key_visits, field_count - 1);
    }
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
    let report = registry.register(&test_connection_id("ext"), tool);

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

/// Ensures harness-owned tools retain distinct policy groups rather than
/// inheriting an implicit shared internal-tool capability.
#[test]
fn internal_tools_preserve_independent_registration_groups() {
    let mut registry = ToolRegistry::new();
    let mut self_tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }));
    self_tool.name = ToolName::new("compact");
    self_tool.enabled_by_default = false;
    let mut cross_tool = self_tool.clone();
    cross_tool.name = ToolName::new("agent_compact");

    registry.register_internal_with_group(
        &test_connection_id("harness"),
        self_tool,
        ToolGroup {
            name: tau_proto::ToolGroupName::new("compaction"),
            prompt_fragment: None,
        },
    );
    registry.register_internal_with_group(
        &test_connection_id("harness"),
        cross_tool,
        ToolGroup {
            name: tau_proto::ToolGroupName::new("cross_agent_compaction"),
            prompt_fragment: None,
        },
    );

    assert_eq!(
        registry
            .resolve_provider("compact")
            .and_then(|provider| provider.tool_group.as_ref())
            .map(|group| group.name.as_str()),
        Some("compaction")
    );
    assert_eq!(
        registry
            .resolve_provider("agent_compact")
            .and_then(|provider| provider.tool_group.as_ref())
            .map(|group| group.name.as_str()),
        Some("cross_agent_compaction")
    );
}

/// Harness-owned tools are authoritative even if an extension registered the
/// same name first, and the displaced connection no longer retains ownership.
#[test]
fn internal_registration_preempts_extension_owner() {
    let mut registry = ToolRegistry::new();
    let mut tool = strict_tool(serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }));
    tool.name = ToolName::new("message");

    assert!(
        registry
            .register(&test_connection_id("extension"), tool.clone())
            .errors
            .is_empty()
    );
    assert!(
        registry
            .register_internal(&test_connection_id("harness"), tool)
            .errors
            .is_empty()
    );

    let provider = registry
        .resolve_provider("message")
        .expect("internal provider replaces extension provider");
    assert_eq!(provider.connection_id.as_str(), "harness");
    assert_eq!(provider.kind, ToolProviderKind::Internal);
    assert!(
        registry
            .unregister_connection(&test_connection_id("extension"))
            .is_empty()
    );
    assert_eq!(
        registry
            .resolve_provider("message")
            .map(|provider| provider.connection_id.as_str()),
        Some("harness")
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

/// Builds a validated connection identifier used by this test module.
fn test_connection_id(value: impl AsRef<str>) -> tau_proto::ConnectionId {
    tau_proto::ConnectionId::parse(value.as_ref())
        .expect("test connection id must satisfy the identifier grammar")
}
