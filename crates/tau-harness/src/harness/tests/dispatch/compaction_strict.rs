//! Strict in-process provider compaction regressions.

use super::*;

/// The real in-process provider/tool route must reject any unbalanced compact
/// timeline, accept the normalized post-tool threshold request, and complete a
/// later queued user prompt after the recovered continuation.
#[test]
fn strict_provider_vertical_slice_accepts_closed_post_tool_compaction() {
    let td = TempDir::new().expect("tempdir");
    let mut h = strict_compaction_provider_harness(td.path().join("state")).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "first tool round".to_owned())
        .expect("submit first prompt");
    h.submit_user_prompt(test_session_id("s1"), "later queued prompt".to_owned())
        .expect("queue later prompt");

    let error = h
        .run_event_loop(None, false)
        .expect_err("test provider disconnects after completing the later prompt");
    assert!(matches!(
        error,
        HarnessError::Participant(message) if message == "provider disconnected"
    ));
    let events = event_log_events(&h);
    let starts = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some((index, started)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 1, "the fixture permits one post-tool pass");
    let (start_index, started) = starts[0];
    let closed_tool_result_index = events
        .iter()
        .rposition(|event| {
            matches!(
                event,
                Event::ProviderToolResult(result) | Event::ToolResult(result)
                    if result.call_id.as_str() == "call-strict-echo"
                        && result.kind == tau_proto::ToolResultKind::Final
            ) || matches!(
                event,
                Event::ProviderToolError(error) | Event::ToolError(error)
                    if error.call_id.as_str() == "call-strict-echo"
            )
        })
        .expect("closed strict tool result");
    assert!(
        closed_tool_result_index < start_index,
        "the one automatic cut follows the closed tool-result round"
    );
    assert!(
        matches!(started.cut, tau_proto::AgentHead::Node(_)) && started.resume_through.is_some(),
        "the automatic pass retains a non-root closed prefix and continuation"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::AgentCompacted(_)))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::AgentStandaloneCompactionFailed(_)))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::ProviderResponseFinished(response)
            if response.output_items.iter().any(|item| matches!(
                item,
                ContextItem::Message(message)
                    if message.content.iter().any(|part| matches!(
                        part,
                        ContentPart::Text { text } if text == "later prompt complete"
                    ))
            ))
    )));
}
