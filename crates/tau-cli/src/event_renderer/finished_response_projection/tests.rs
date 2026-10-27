use super::super::terminal_tool_calls_tests;
use super::super::tests::renderer_for_agent_id_tests;
use super::*;

/// A continuation-cap error must remain visible beside the accepted partial
/// answer rather than disappearing because the response has output.
#[test]
fn continuation_limit_renders_error_alongside_partial_output() {
    let renderer = renderer_for_agent_id_tests();
    let mut response = terminal_tool_calls_tests::finished(
        vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "Stable partial".to_owned(),
            }],
            phase: Some(tau_proto::MessagePhase::PartialAnswer),
            responses_raw_json: None,
        })],
        tau_proto::ProviderStopReason::Error,
    );
    response.error = Some("Provider response continuation limit reached".to_owned());
    let calls = TerminalToolCalls::from_finished(&response);
    let block = renderer
        .stage_finished_placeholder(&response, &calls)
        .expect("visible error");
    let text: String = block
        .content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();
    assert!(text.contains("continuation limit reached"));
    assert_eq!(response.output_items.len(), 1);
}
