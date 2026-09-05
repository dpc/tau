use super::*;

/// Session inspection uses the protocol-owned parser rather than maintaining a
/// divergent shell-outcome interpretation.
#[test]
fn inspection_alias_projects_canonical_terminal() {
    let event = Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
        call_id: "call".into(),
        tool_name: tau_proto::ToolName::new("shell"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Map(vec![(
            CborValue::Text("status".into()),
            CborValue::Integer(0.into()),
        )]),
        display: None,
        originator: tau_proto::PromptOriginator::User,
    });

    let outcome = ShellOutcome::from_terminal_event(&event).expect("canonical shell outcome");
    assert_eq!(outcome.source(), ShellOutcomeSource::ToolResult);
    assert_eq!(outcome.termination_reason(), ShellTerminationReason::Exit);
    assert_eq!(outcome.exit_code(), Some(0));
}
