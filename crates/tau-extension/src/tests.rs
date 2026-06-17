use std::io::Cursor;

use tau_proto::{
    ActionSchema, ActionSchemaPublished, ClientKind, Event, EventName, EventSelector,
    ExtensionName, HarnessInputMessage, HarnessInputReader, Hello, Intercept, InterceptionPriority,
    PeerOutputWriter, Ready, Subscribe, ToolName, ToolRegister, ToolSpec, ToolType,
};

use super::*;

#[derive(serde::Deserialize, PartialEq, Debug)]
#[serde(default, deny_unknown_fields)]
struct Demo {
    n: u32,
}

impl Default for Demo {
    fn default() -> Self {
        Self { n: 1 }
    }
}

fn handshake_messages(handshake: Handshake) -> Vec<HarnessInputMessage> {
    let mut bytes = Vec::new();
    {
        let mut writer = PeerOutputWriter::new(&mut bytes);
        handshake.run(&mut writer).expect("handshake should encode");
    }

    let mut reader = HarnessInputReader::new(Cursor::new(bytes));
    let mut messages = Vec::new();
    while let Some(message) = reader
        .read_message()
        .expect("handshake message should decode")
    {
        messages.push(message);
    }
    messages
}

fn tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new("demo_tool"),
        model_visible_name: None,
        description: Some("Demo tool".to_owned()),
        tool_type: ToolType::Function,
        parameters: None,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
    }
}

fn cbor_from_json(json: serde_json::Value) -> tau_proto::CborValue {
    tau_proto::json_to_cbor(&json)
}

#[test]
fn parse_config_returns_typed_struct() {
    let value = cbor_from_json(serde_json::json!({ "n": 7 }));
    let cfg: Demo = parse_config(&value).expect("parse");
    assert_eq!(cfg, Demo { n: 7 });
}

#[test]
fn parse_config_error_strips_debug_wrapper() {
    let value = cbor_from_json(serde_json::json!({ "wrong": 7 }));
    let err = parse_config::<Demo>(&value).expect_err("should fail");
    // No `Semantic(None, "…")` wrapping: just the message.
    assert!(!err.starts_with("Semantic"), "got: {err}");
    assert!(err.contains("unknown field"), "got: {err}");
    assert!(err.contains("wrong"), "got: {err}");
}

/// Ensures repeated same-priority intercepts become one wire registration.
#[test]
fn handshake_accumulates_intercepts_into_one_message() {
    let priority = InterceptionPriority::new(5);
    let messages = handshake_messages(
        Handshake::tool("demo")
            .intercept(EventSelector::Exact(EventName::TOOL_STARTED), priority)
            .intercept(EventSelector::Prefix("tool.".to_owned()), priority),
    );

    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[1],
        HarnessInputMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(EventName::TOOL_STARTED),
                EventSelector::Prefix("tool.".to_owned()),
            ],
            priority,
        })
    );
}

/// Ensures mixed-priority intercept registrations return a handshake error
/// instead of panicking or silently replacing an earlier policy in the harness.
#[test]
fn handshake_rejects_mixed_intercept_priorities() {
    let handshake = Handshake::tool("demo")
        .intercept(
            EventSelector::Exact(EventName::TOOL_STARTED),
            InterceptionPriority::new(1),
        )
        .intercept(
            EventSelector::Prefix("tool.".to_owned()),
            InterceptionPriority::new(2),
        );
    let mut bytes = Vec::new();
    let mut writer = PeerOutputWriter::new(&mut bytes);

    let error = handshake
        .run(&mut writer)
        .expect_err("mixed intercept priorities should fail");

    assert!(matches!(
        error,
        HandshakeError::MixedInterceptPriorities {
            existing,
            requested,
        } if existing == InterceptionPriority::new(1)
            && requested == InterceptionPriority::new(2)
    ));
    assert!(bytes.is_empty());
}

/// Ensures callers that want immediate validation can use the fallible builder
/// method instead of waiting for `run`.
#[test]
fn handshake_try_intercept_rejects_mixed_priorities_immediately() {
    let result = Handshake::tool("demo")
        .try_intercept(
            EventSelector::Exact(EventName::TOOL_STARTED),
            InterceptionPriority::new(1),
        )
        .expect("first intercept should be accepted")
        .try_intercept(
            EventSelector::Prefix("tool.".to_owned()),
            InterceptionPriority::new(2),
        );
    let error = match result {
        Ok(_) => panic!("mixed intercept priorities should fail"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        HandshakeError::MixedInterceptPriorities {
            existing,
            requested,
        } if existing == InterceptionPriority::new(1)
            && requested == InterceptionPriority::new(2)
    ));
}

/// Ensures the handshake writes the full protocol prelude in wire order.
#[test]
fn handshake_writes_full_prelude_in_order() {
    let priority = InterceptionPriority::new(3);
    let tool = tool_spec();
    let messages = handshake_messages(
        Handshake::with_kind("demo", ClientKind::Tool)
            .subscribe([EventName::TOOL_STARTED])
            .intercept(EventSelector::Prefix("tool.".to_owned()), priority)
            .register_tool(tool.clone())
            .publish_actions(ActionSchema::default())
            .ready_message("ready"),
    );

    assert_eq!(messages.len(), 6);

    assert_eq!(
        messages[0],
        HarnessInputMessage::Hello(Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "demo".into(),
            client_kind: ClientKind::Tool,
        })
    );
    assert_eq!(
        messages[1],
        HarnessInputMessage::Subscribe(Subscribe {
            selectors: vec![EventSelector::Exact(EventName::TOOL_STARTED)],
        })
    );
    assert_eq!(
        messages[2],
        HarnessInputMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix("tool.".to_owned())],
            priority,
        })
    );
    assert_eq!(
        messages[3],
        HarnessInputMessage::emit(Event::ToolRegister(ToolRegister {
            tool,
            tool_group: None,
            prompt_fragment: None,
        }))
    );
    assert_eq!(
        messages[4],
        HarnessInputMessage::emit(Event::ActionSchemaPublished(ActionSchemaPublished {
            extension_name: ExtensionName::default(),
            instance_id: 0.into(),
            schema: ActionSchema::default(),
        }))
    );
    assert_eq!(
        messages[5],
        HarnessInputMessage::Ready(Ready {
            message: Some("ready".to_owned()),
        })
    );
}

/// Ensures empty subscriptions are omitted and ready messages are preserved.
#[test]
fn handshake_omits_empty_subscribe_and_preserves_ready_message() {
    let messages = handshake_messages(Handshake::tool("demo").ready_message("ready"));

    assert_eq!(messages.len(), 2);
    assert!(matches!(messages[0], HarnessInputMessage::Hello(_)));
    assert_eq!(
        messages[1],
        HarnessInputMessage::Ready(Ready {
            message: Some("ready".to_owned()),
        })
    );
}
