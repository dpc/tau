use super::*;

/// The bounded parser accepts approved coherent shapes and rejects
/// malformed, contradictory, or duplicated recognized fields.
#[test]
fn parser_enforces_approved_coherence() {
    let map = |entries| CborValue::Map(entries);
    let field = |key: &str, value| (CborValue::Text(key.into()), value);
    let parsed = ShellProcessOutcome::from_cbor(
        ShellProcessOutcomeSource::ToolResult,
        &map(vec![field("status", CborValue::Integer(0.into()))]),
    )
    .expect("legacy result exit");
    assert!(parsed.success());
    assert_eq!(parsed.termination_reason(), ShellTerminationReason::Exit);

    let timeout = ShellProcessOutcome::from_cbor(
        ShellProcessOutcomeSource::ToolResult,
        &map(vec![
            field("timed_out", CborValue::Bool(true)),
            field("termination_reason", CborValue::Text("timeout".into())),
        ]),
    )
    .expect("timeout");
    assert!(timeout.timed_out());
    assert!(!timeout.success());

    assert!(
        ShellProcessOutcome::from_cbor(
            ShellProcessOutcomeSource::ToolErrorDetails,
            &map(vec![field(
                "termination_reason",
                CborValue::Text("start_error".into()),
            )]),
        )
        .is_some()
    );

    let signal = ShellProcessOutcome::from_cbor(
        ShellProcessOutcomeSource::ToolResult,
        &map(vec![
            field("status", CborValue::Integer(128.into())),
            field("signal", CborValue::Integer(15.into())),
            field("termination_reason", CborValue::Text("signal".into())),
        ]),
    )
    .expect("signal with status");
    assert_eq!(signal.exit_code(), Some(128));
    assert_eq!(signal.signal(), Some(15));

    let timeout_with_auxiliaries = ShellProcessOutcome::from_cbor(
        ShellProcessOutcomeSource::ToolResult,
        &map(vec![
            field("status", CborValue::Integer(i32::MAX.into())),
            field("signal", CborValue::Integer(9.into())),
            field("timed_out", CborValue::Bool(true)),
            field("termination_reason", CborValue::Text("timeout".into())),
        ]),
    )
    .expect("timeout auxiliaries");
    assert_eq!(timeout_with_auxiliaries.exit_code(), Some(i32::MAX));
    assert_eq!(timeout_with_auxiliaries.signal(), Some(9));

    let unknown = ShellProcessOutcome::from_cbor(
        ShellProcessOutcomeSource::ToolResult,
        &map(vec![
            field("status", CborValue::Integer(i32::MIN.into())),
            field("signal", CborValue::Integer(6.into())),
            field("timed_out", CborValue::Bool(true)),
            field("termination_reason", CborValue::Text("unknown".into())),
        ]),
    )
    .expect("unknown auxiliaries");
    assert_eq!(unknown.exit_code(), Some(i32::MIN));
    assert_eq!(unknown.signal(), Some(6));
    assert!(unknown.timed_out());

    let start_error_with_auxiliaries = ShellProcessOutcome::from_cbor(
        ShellProcessOutcomeSource::ToolErrorDetails,
        &map(vec![
            field("status", CborValue::Integer(7.into())),
            field("signal", CborValue::Integer(9.into())),
            field("timed_out", CborValue::Bool(true)),
            field("termination_reason", CborValue::Text("start_error".into())),
        ]),
    )
    .expect("start error auxiliaries");
    assert_eq!(start_error_with_auxiliaries.exit_code(), Some(7));
    assert!(
        ShellProcessOutcome::from_cbor(
            ShellProcessOutcomeSource::ToolResult,
            &map(vec![field(
                "termination_reason",
                CborValue::Text("start_error".into()),
            )]),
        )
        .is_none()
    );

    let legacy_with_false = map(vec![
        field("status", CborValue::Integer(0.into())),
        field("timed_out", CborValue::Bool(false)),
    ]);
    assert!(
        ShellProcessOutcome::from_cbor(ShellProcessOutcomeSource::ToolResult, &legacy_with_false)
            .is_none()
    );
    let error_legacy = map(vec![field("status", CborValue::Integer(0.into()))]);
    assert!(
        ShellProcessOutcome::from_cbor(ShellProcessOutcomeSource::ToolErrorDetails, &error_legacy)
            .is_none()
    );

    for malformed in [
        map(vec![field("status", CborValue::Text("0".into()))]),
        map(vec![
            field("status", CborValue::Integer(0.into())),
            field("status", CborValue::Integer(1.into())),
        ]),
        map(vec![
            field("status", CborValue::Integer(0.into())),
            field("termination_reason", CborValue::Text("signal".into())),
        ]),
        map(vec![field(
            "termination_reason",
            CborValue::Text("future".into()),
        )]),
    ] {
        assert!(
            ShellProcessOutcome::from_cbor(ShellProcessOutcomeSource::ToolResult, &malformed)
                .is_none(),
            "malformed outcome must be unavailable"
        );
    }
}

/// Exit-status integers accept both signed 32-bit endpoints and reject
/// either overflow direction exactly.
#[test]
fn integer_fields_enforce_i32_bounds() {
    let outcome = |value: i64| {
        ShellProcessOutcome::from_cbor(
            ShellProcessOutcomeSource::ToolResult,
            &CborValue::Map(vec![(
                CborValue::Text("status".into()),
                CborValue::Integer(value.into()),
            )]),
        )
    };
    assert_eq!(
        outcome(i64::from(i32::MIN)).expect("minimum").exit_code(),
        Some(i32::MIN)
    );
    assert_eq!(
        outcome(i64::from(i32::MAX)).expect("maximum").exit_code(),
        Some(i32::MAX)
    );
    assert!(outcome(i64::from(i32::MIN) - 1).is_none());
    assert!(outcome(i64::from(i32::MAX) + 1).is_none());
}

/// Duplicate recognized fields are ambiguous, while duplicate unknown keys
/// remain irrelevant even in a large bounded producer map.
#[test]
fn duplicate_detection_is_bounded_and_projection_specific() {
    let mut large = (0..20_000)
        .map(|index| {
            (
                CborValue::Text(format!("irrelevant-{index}")),
                CborValue::Null,
            )
        })
        .collect::<Vec<_>>();
    large.extend([
        (
            CborValue::Text("unknown".into()),
            CborValue::Integer(1.into()),
        ),
        (
            CborValue::Text("unknown".into()),
            CborValue::Integer(2.into()),
        ),
        (
            CborValue::Text("status".into()),
            CborValue::Integer(i32::MIN.into()),
        ),
    ]);
    assert!(
        ShellProcessOutcome::from_cbor(
            ShellProcessOutcomeSource::ToolResult,
            &CborValue::Map(large)
        )
        .is_some()
    );
    let duplicate_status = CborValue::Map(vec![
        (
            CborValue::Text("status".into()),
            CborValue::Integer(0.into()),
        ),
        (
            CborValue::Text("status".into()),
            CborValue::Integer(1.into()),
        ),
    ]);
    assert!(
        ShellProcessOutcome::from_cbor(ShellProcessOutcomeSource::ToolResult, &duplicate_status)
            .is_none()
    );
}

/// Canonical foreground/background terminal families must select the approved
/// source field and reject synthetic foreground placeholders.
#[test]
fn terminal_event_mapping_preserves_source_and_omits_placeholders() {
    let result = CborValue::Map(vec![(
        CborValue::Text("status".into()),
        CborValue::Integer(0.into()),
    )]);
    let error = CborValue::Map(vec![(
        CborValue::Text("termination_reason".into()),
        CborValue::Text("start_error".into()),
    )]);
    let tool_result = |kind| {
        Event::ProviderToolResult(crate::ToolResult {
            presentation: Default::default(),
            call_id: "call".into(),
            tool_name: crate::ToolName::new("shell"),
            tool_type: crate::ToolType::Function,
            result: result.clone(),
            provider_content: Vec::new(),
            kind,
            display: None,
            originator: crate::PromptOriginator::User,
        })
    };
    let foreground = ShellProcessOutcome::from_terminal_event(&tool_result(ToolResultKind::Final))
        .expect("foreground result");
    assert!(matches!(
        foreground.source(),
        ShellProcessOutcomeSource::ToolResult
    ));
    assert_eq!(foreground.exit_code(), Some(0));
    assert!(
        ShellProcessOutcome::from_terminal_event(&tool_result(
            ToolResultKind::BackgroundPlaceholder
        ))
        .is_none()
    );

    let foreground_error = Event::ProviderToolError(crate::ToolError {
        presentation: Default::default(),
        call_id: "call".into(),
        tool_name: crate::ToolName::new("shell"),
        tool_type: crate::ToolType::Function,
        message: "start".into(),
        details: Some(error.clone()),
        display: None,
        originator: crate::PromptOriginator::User,
    });
    assert!(matches!(
        ShellProcessOutcome::from_terminal_event(&foreground_error)
            .expect("foreground error")
            .source(),
        ShellProcessOutcomeSource::ToolErrorDetails
    ));

    let background = Event::ToolBackgroundResult(crate::ToolBackgroundResult {
        call_id: "call".into(),
        tool_name: crate::ToolName::new("shell"),
        tool_type: crate::ToolType::Function,
        result,
        display: None,
        originator: crate::PromptOriginator::User,
    });
    assert!(matches!(
        ShellProcessOutcome::from_terminal_event(&background)
            .expect("background result")
            .source(),
        ShellProcessOutcomeSource::ToolResult
    ));

    let background_error = Event::ToolBackgroundError(crate::ToolBackgroundError {
        call_id: "call".into(),
        tool_name: crate::ToolName::new("shell"),
        tool_type: crate::ToolType::Function,
        message: "start".into(),
        details: Some(error),
        display: None,
        originator: crate::PromptOriginator::User,
    });
    assert!(matches!(
        ShellProcessOutcome::from_terminal_event(&background_error)
            .expect("background error")
            .source(),
        ShellProcessOutcomeSource::ToolErrorDetails
    ));
}
