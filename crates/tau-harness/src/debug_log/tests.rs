use tau_proto::{
    ActionInvocationId, AgentPromptId, CborValue, ExtensionInstanceId, ExtensionName,
    HarnessInputMessage, ModelId, PromptOriginator, ProviderResponseFinished,
    ProviderResponseTextDelta, ProviderResponseUpdated, ProviderTokenUsage, ReasoningTextKind,
    SessionId,
};

use super::*;
use crate::event::HarnessEvent;

fn read_lines(path: &Path) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(path).expect("read events.jsonl");
    raw.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("parse line"))
        .collect()
}

#[test]
fn published_line_preserves_enriched_token_usage() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");
    let model: ModelId = "openai/gpt-5".parse().expect("model id");
    let event = Event::ProviderResponseFinished(ProviderResponseFinished {
        agent_prompt_id: AgentPromptId::from("sp-0"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        originator: PromptOriginator::User,
        usage: Some(ProviderTokenUsage {
            model: Some(model),
            prompt_sent_tokens: 1000,
            prompt_cached_tokens: 800,
            response_received_tokens: 42,
            stats: tau_proto::TokenUsageStats::default(),
        }),
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    });
    log.log_published_event(
        Some(&ConnectionId::from("conn-1")),
        &event,
        UnixMicros::now(),
    );

    let lines = read_lines(log.path());
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(line["type"], "published");
    assert_eq!(line["event_name"], "provider.response_finished");
    assert_eq!(line["source"], "conn-1");
    let usage = &line["event"]["payload"]["usage"];
    assert_eq!(usage["prompt_sent_tokens"], 1000);
    assert_eq!(usage["prompt_cached_tokens"], 800);
    assert_eq!(usage["response_received_tokens"], 42);
    assert_eq!(usage["model"], "openai/gpt-5");
}

#[test]
fn published_line_compacts_long_strings() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");
    let event = Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: AgentPromptId::from("sp-0"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: vec![
            ProviderResponseTextDelta::Message {
                output_index: 0,
                text: "x".repeat(101),
                phase: None,
            },
            ProviderResponseTextDelta::ReasoningText {
                output_index: 1,
                kind: ReasoningTextKind::Summary,
                text: format!("{}{}{}", "α".repeat(30), "middle", "ω".repeat(30)),
            },
        ],
        compaction: None,
        status: None,
        semantic_output: None,
        originator: PromptOriginator::User,
    });

    log.log_published_event(None, &event, UnixMicros::now());

    let lines = read_lines(log.path());
    assert_eq!(lines.len(), 1);
    let payload = &lines[0]["event"]["payload"];
    assert_eq!(
        payload["deltas"][0]["text"],
        "xxxxxxxxxxxxxxxxxxxx┄total 101┄xxxxxxxxxxxxxxxxxxxx"
    );
    assert_eq!(
        payload["deltas"][1]["text"],
        "αααααααααα┄total 126┄ωωωωωωωωωω"
    );
}

#[test]
fn published_action_invoke_redacts_gmail_oauth_redirect_url() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");
    let event = Event::ActionInvoke(tau_proto::ActionInvoke {
        invocation_id: ActionInvocationId::from("action-1"),
        session_id: SessionId::from("s1"),
        extension_name: ExtensionName::from("tau-ext-pim"),
        instance_id: ExtensionInstanceId::from(0),
        action_id: "email.auth.google.finish".to_owned(),
        raw_line: "/email auth google finish work http://127.0.0.1:54321/?state=state-secret&code=auth-code-secret".to_owned(),
        argv: vec![
            "work".to_owned(),
            "http://127.0.0.1:54321/?state=state-secret&code=auth-code-secret".to_owned(),
        ],
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("account".to_owned()),
                CborValue::Text("work".to_owned()),
            ),
            (
                CborValue::Text("redirect_url".to_owned()),
                CborValue::Text(
                    "http://127.0.0.1:54321/?state=state-secret&code=auth-code-secret"
                        .to_owned(),
                ),
            ),
        ]),
    });

    log.log_published_event(None, &event, UnixMicros::now());

    let raw = std::fs::read_to_string(log.path()).expect("read events.jsonl");
    assert!(!raw.contains("auth-code-secret"));
    assert!(!raw.contains("state-secret"));
    assert!(raw.contains("<redirect-url-redacted>"));
    assert!(raw.contains("\"arguments\":\"<redacted>\""));
}

#[test]
fn compact_debug_string_keeps_short_strings() {
    assert_eq!(compact_debug_string(&"x".repeat(100)), "x".repeat(100));
}

#[test]
fn transient_from_connection_events_are_not_logged_twice() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");
    let event = Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: AgentPromptId::from("sp-0"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: vec![ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "partial".to_owned(),
            phase: None,
        }],
        compaction: None,
        status: None,
        semantic_output: None,
        originator: PromptOriginator::User,
    });

    log.log_harness_event(&HarnessEvent::FromConnection {
        connection_id: ConnectionId::from("conn-1"),
        message: Box::new(HarnessInputMessage::emit(event)),
    });

    let lines = read_lines(log.path());
    assert!(
        lines.is_empty(),
        "transient streaming events are logged on publish; the raw inbound copy is redundant"
    );
}
