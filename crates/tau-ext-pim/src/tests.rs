use std::collections::BTreeMap;
use std::rc::Rc;

use serde::Deserialize;
use tau_proto::{
    EventName, EventSelector, HarnessInputMessage, HarnessInputReader, PeerOutputWriter,
};

use super::*;

/// A failed PIM reconfigure may be an attempted policy revocation. Ensure the
/// wrapper does not keep serving calls from a previously accepted email or
/// calendar module state after reporting the new configuration as rejected.
#[test]
fn rejected_reconfigure_clears_previous_module_state() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let storage = Rc::new(storage::FsStorage::new(temp.path().join("storage")));
    let mut runtime = RuntimeState::default();
    runtime
        .configure(
            configure(CborValue::Map(vec![]), temp.path()),
            storage.clone(),
        )
        .expect("initial default config is accepted");

    let rejected = CborValue::Map(vec![
        (
            CborValue::Text("email".to_owned()),
            CborValue::Map(Vec::new()),
        ),
        (
            CborValue::Text("calendar".to_owned()),
            CborValue::Map(vec![(
                CborValue::Text("unknown".to_owned()),
                CborValue::Bool(true),
            )]),
        ),
    ]);
    assert!(
        runtime
            .configure(configure(rejected, temp.path()), storage)
            .is_err()
    );

    let event = runtime
        .dispatch_tool(tau_proto::ToolStarted {
            call_id: tau_proto::ToolCallId::new("call-email"),
            tool_name: tau_proto::ToolName::new("email_list_folders"),
            arguments: CborValue::Map(vec![]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        })
        .expect("email tool is handled by PIM");

    let Event::ToolError(error) = event else {
        panic!("rejected email module should return a tool error")
    };
    assert!(
        error
            .display
            .expect("display")
            .status_text
            .contains("configuration was rejected")
    );

    let event = runtime
        .dispatch_tool(tau_proto::ToolStarted {
            call_id: tau_proto::ToolCallId::new("call-calendar"),
            tool_name: tau_proto::ToolName::new("calendar_list_calendars"),
            arguments: CborValue::Map(vec![]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        })
        .expect("calendar tool is handled by PIM");

    let Event::ToolError(error) = event else {
        panic!("rejected calendar module should return a tool error")
    };
    assert!(
        error
            .display
            .expect("display")
            .status_text
            .contains("configuration was rejected")
    );
}

/// Legacy email-shaped configs still pass through the PIM wrapper. If that
/// fallback email configuration fails after a prior successful configure, the
/// wrapper must reject both modules instead of leaving stale calendar access.
#[test]
fn rejected_legacy_fallback_reconfigure_clears_calendar_state() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let storage = Rc::new(storage::FsStorage::new(temp.path().join("storage")));
    let mut runtime = RuntimeState::default();
    runtime
        .configure(
            configure(CborValue::Map(vec![]), temp.path()),
            storage.clone(),
        )
        .expect("initial default config is accepted");

    let rejected = CborValue::Map(vec![(
        CborValue::Text("accounts".to_owned()),
        CborValue::Text("not an email account list".to_owned()),
    )]);
    assert!(
        runtime
            .configure(configure(rejected, temp.path()), storage)
            .is_err()
    );

    let event = runtime
        .dispatch_tool(tau_proto::ToolStarted {
            call_id: tau_proto::ToolCallId::new("call-calendar"),
            tool_name: tau_proto::ToolName::new("calendar_list_calendars"),
            arguments: CborValue::Map(vec![]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        })
        .expect("calendar tool is handled by PIM");

    let Event::ToolError(error) = event else {
        panic!("rejected calendar module should return a tool error")
    };
    assert!(
        error
            .display
            .expect("display")
            .status_text
            .contains("configuration was rejected")
    );
}

fn configure(config: CborValue, state_root: &std::path::Path) -> tau_proto::Configure {
    tau_proto::Configure {
        config,
        instance_name: None,
        state_dir: Some(state_root.join("state")),
        secrets: BTreeMap::new(),
    }
}

#[test]
fn self_knowledge_pim_example_matches_extension_config_shape() {
    #[derive(Deserialize)]
    struct HarnessExample {
        extensions: BTreeMap<String, ExtensionExample>,
    }

    #[derive(Deserialize)]
    struct ExtensionExample {
        config: PimExtensionConfig,
    }

    let mut harness: HarnessExample =
        serde_yaml_ng::from_str(include_str!("../config/self-knowledge.harness.yaml"))
            .expect("self-knowledge PIM example parses as YAML");
    let pim = harness
        .extensions
        .remove("std-pim")
        .expect("std-pim example exists")
        .config;

    pim.email
        .expect("email example")
        .validate()
        .expect("email config validates");
    pim.calendar
        .expect("calendar example")
        .validate()
        .expect("calendar config validates");
}

#[test]
fn action_schema_contains_email_and_calendar_roots() {
    let roots = action_schema()
        .roots
        .into_iter()
        .map(|root| root.name)
        .collect::<Vec<_>>();

    assert_eq!(roots, vec!["/email", "/calendar"]);
}

/// Calendar Google auth intentionally remains device-flow based and its finish
/// action does not accept Gmail's pasted redirect URL argument.
#[test]
fn calendar_google_auth_schema_remains_device_flow_shape() {
    let schema = action_schema();
    let start = schema
        .parse_line("/calendar auth google start google")
        .expect("calendar auth start parses");
    assert_eq!(start.action_id, "calendar.auth.google.start");
    assert_eq!(start.argv, vec!["google".to_owned()]);

    let finish = schema
        .parse_line("/calendar auth google finish google")
        .expect("calendar auth finish parses");
    assert_eq!(finish.action_id, "calendar.auth.google.finish");
    assert_eq!(finish.argv, vec!["google".to_owned()]);

    assert!(
        schema
            .parse_line(
                "/calendar auth google finish google http://127.0.0.1:54321/?state=s&code=c",
            )
            .is_err(),
        "calendar finish must not accept Gmail redirect URL arguments"
    );
}

/// PIM subscribes to `tool.started` to receive its own email/calendar
/// calls, but the harness event stream can also contain starts for
/// tools owned by other extensions. Those foreign calls must be ignored
/// instead of producing terminal tool errors that race with the real
/// provider result.
#[test]
fn ignores_tool_started_for_tools_owned_by_other_extensions() {
    let mut runtime = RuntimeState::default();
    for tool_name in ["read", email::TOOL_NAME, calendar::TOOL_NAME] {
        let invoke = tau_proto::ToolStarted {
            call_id: tau_proto::ToolCallId::new(format!("call-{tool_name}")),
            tool_name: tau_proto::ToolName::new(tool_name),
            arguments: CborValue::Map(vec![]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        };

        assert!(runtime.dispatch_tool(invoke).is_none());
    }
}

#[test]
fn handshake_registers_email_and_calendar_tools() {
    let mut bytes = Vec::new();
    let handshake = tau_extension::Handshake::tool("tau-ext-pim").subscribe([
        tau_proto::EventName::TOOL_STARTED,
        tau_proto::EventName::ACTION_INVOKE,
    ]);
    let handshake = register_tools_with_prompt_fragment(
        handshake,
        email::email_tool_specs(),
        tau_proto::ToolGroupName::new("email"),
        "email_read",
        email::email_prompt_fragment(),
    );
    let handshake = register_tools_with_prompt_fragment(
        handshake,
        calendar::calendar_tool_specs(),
        tau_proto::ToolGroupName::new("calendar"),
        "calendar_get",
        calendar::calendar_prompt_fragment(),
    );

    handshake
        .publish_actions(action_schema())
        .ready_message("pim extension ready")
        .run(&mut PeerOutputWriter::new(&mut bytes))
        .expect("handshake writes");

    let mut reader = HarnessInputReader::new(bytes.as_slice());
    let mut tools = Vec::new();
    let mut prompt_tools = Vec::new();
    let mut per_tool_prompt_tools = Vec::new();
    let mut saw_subscription = false;
    while let Some(frame) = reader.read_message().expect("frame decodes") {
        match frame {
            HarnessInputMessage::Subscribe(subscribe) => {
                saw_subscription = subscribe.selectors
                    == vec![
                        EventSelector::Exact(EventName::TOOL_STARTED),
                        EventSelector::Exact(EventName::ACTION_INVOKE),
                    ];
            }
            HarnessInputMessage::Emit(emit)
                if matches!(emit.event.as_ref(), Event::ToolRegister(_)) =>
            {
                let Event::ToolRegister(register) = *emit.event else {
                    unreachable!();
                };
                if register.prompt_fragment.is_some() {
                    per_tool_prompt_tools.push(register.tool.name.clone());
                }
                if register
                    .tool_group
                    .as_ref()
                    .and_then(|group| group.prompt_fragment.as_ref())
                    .is_some()
                {
                    prompt_tools.push(
                        register
                            .tool_group
                            .as_ref()
                            .expect("group with prompt")
                            .name
                            .clone(),
                    );
                }
                tools.push(register.tool.name);
            }
            _ => {}
        }
    }

    assert!(saw_subscription);
    assert!(
        tools
            .iter()
            .any(|tool| tool.as_str() == "email_list_folders")
    );
    assert!(tools.iter().any(|tool| tool.as_str() == "email_send"));
    assert!(
        tools
            .iter()
            .any(|tool| tool.as_str() == "calendar_list_calendars")
    );
    assert!(tools.iter().any(|tool| tool.as_str() == "calendar_respond"));
    assert!(prompt_tools.iter().any(|group| group.as_str() == "email"));
    assert!(
        prompt_tools
            .iter()
            .any(|group| group.as_str() == "calendar")
    );
    assert!(
        per_tool_prompt_tools
            .iter()
            .any(|tool| tool.as_str() == "email_read")
    );
    assert!(
        per_tool_prompt_tools
            .iter()
            .any(|tool| tool.as_str() == "calendar_get")
    );
    assert!(!tools.iter().any(|tool| tool.as_str() == email::TOOL_NAME));
    assert!(
        !tools
            .iter()
            .any(|tool| tool.as_str() == calendar::TOOL_NAME)
    );
    assert_eq!(tools.len(), 18);
}
