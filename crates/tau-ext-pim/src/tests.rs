//! Boundary-focused coverage follows `testing.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::io as path_std_io;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tau_proto::{
    EventName, EventSelector, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage,
    HarnessOutputWriter,
};

use super::*;

/// Thread-safe test writer that captures startup frames emitted by tau-client.
#[derive(Clone, Default)]
struct SharedWriter {
    /// Shared buffer containing encoded harness-input frames.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("writer lock").clone()
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().expect("writer lock").extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

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

    let invoke = tau_proto::ToolStarted {
        call_id: tau_proto::ToolCallId::new("call-email"),
        tool_name: tau_proto::ToolName::new("email_list_folders"),
        arguments: CborValue::Map(vec![]),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    };
    let local_tool_name = invoke.tool_name.clone();
    let event = runtime
        .dispatch_tool(invoke, &local_tool_name)
        .expect("email dispatch succeeds")
        .expect("email tool is handled by PIM");

    let tau_client::ToolTerminalOutcome::Failure(error) = event else {
        panic!("rejected email module should return a tool error")
    };
    assert!(
        error
            .display
            .expect("display")
            .status_text
            .contains("configuration was rejected")
    );

    let invoke = tau_proto::ToolStarted {
        call_id: tau_proto::ToolCallId::new("call-calendar"),
        tool_name: tau_proto::ToolName::new("calendar_list_calendars"),
        arguments: CborValue::Map(vec![]),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    };
    let local_tool_name = invoke.tool_name.clone();
    let event = runtime
        .dispatch_tool(invoke, &local_tool_name)
        .expect("calendar dispatch succeeds")
        .expect("calendar tool is handled by PIM");

    let tau_client::ToolTerminalOutcome::Failure(error) = event else {
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

    let invoke = tau_proto::ToolStarted {
        call_id: tau_proto::ToolCallId::new("call-calendar"),
        tool_name: tau_proto::ToolName::new("calendar_list_calendars"),
        arguments: CborValue::Map(vec![]),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    };
    let local_tool_name = invoke.tool_name.clone();
    let event = runtime
        .dispatch_tool(invoke, &local_tool_name)
        .expect("calendar dispatch succeeds")
        .expect("calendar tool is handled by PIM");

    let tau_client::ToolTerminalOutcome::Failure(error) = event else {
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
        tool_prefix: None,
        config,
        instance_name: tau_proto::ExtensionName::parse("test-extension")
            .expect("test extension name must satisfy the identifier grammar"),
        state_dir: Some(state_root.join("state")),
        secrets: BTreeMap::new(),
        settings_files: Default::default(),
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
    let roots = action_schema_with_accounts(&[], &[])
        .roots
        .into_iter()
        .map(|root| root.name)
        .collect::<Vec<_>>();

    assert_eq!(roots, vec![":email", ":calendar"]);
}

/// Effective interactive OAuth inventories must drive both deep account
/// completions and omitted-argument guidance without changing usage syntax.
#[test]
fn google_auth_schema_uses_sorted_effective_account_names() {
    let schema = action_schema_with_accounts(
        &["zeta".to_owned(), "alpha".to_owned()],
        &["work".to_owned(), "personal".to_owned()],
    );

    let email_arg = action_arg(&schema, &[":email", "auth", "google", "start"], "account");
    assert_eq!(
        email_arg
            .suggestions
            .iter()
            .map(|choice| choice.value.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "zeta"]
    );
    let email_error = schema
        .parse_line(":email auth google start")
        .expect_err("account is required");
    assert_eq!(
        email_error.usage(),
        Some(":email auth google start <account>")
    );
    assert!(email_error.message().contains("alpha, zeta"));

    let calendar_arg = action_arg(
        &schema,
        &[":calendar", "auth", "google", "start"],
        "account",
    );
    assert_eq!(
        calendar_arg
            .suggestions
            .iter()
            .map(|choice| choice.value.as_str())
            .collect::<Vec<_>>(),
        vec!["personal", "work"]
    );
    let calendar_error = schema
        .parse_line(":calendar auth google start")
        .expect_err("account is required");
    assert_eq!(
        calendar_error.usage(),
        Some(":calendar auth google start <account>")
    );
    assert!(calendar_error.message().contains("personal, work"));
}

/// An empty effective OAuth inventory should produce no completion candidates
/// and should explicitly explain that configuration, rather than imply that an
/// undisclosed account name exists.
#[test]
fn google_auth_schema_explains_empty_account_inventory() {
    let schema = action_schema_with_accounts(&[], &[]);

    for (line, path) in [
        (
            ":email auth google start",
            vec![":email", "auth", "google", "start"],
        ),
        (
            ":calendar auth google start",
            vec![":calendar", "auth", "google", "start"],
        ),
    ] {
        assert!(action_arg(&schema, &path, "account").suggestions.is_empty());
        let error = schema.parse_line(line).expect_err("account is required");
        assert!(
            error
                .message()
                .contains("no accounts are available for interactive Google authorization"),
            "{error}"
        );
    }
}

/// Large configured inventories remain within shared action-schema budgets:
/// completion choices and diagnostic account-name repetition are both bounded.
#[test]
fn google_auth_schema_bounds_large_account_inventory() {
    let accounts = (0..(tau_proto::MAX_ACTION_CHOICES + 2))
        .map(|index| format!("id-{index:03}-{}", "a".repeat(121)))
        .collect::<Vec<_>>();
    let schema = action_schema_with_accounts(&accounts, &accounts);
    schema.validate().expect("bounded schema validates");

    let arg = action_arg(&schema, &[":email", "auth", "google", "start"], "account");
    assert_eq!(arg.suggestions.len(), AUTH_ACCOUNT_COMPLETION_LIMIT);
    assert!(arg.description.contains("showing 6 of 130"));
    assert!(!arg.description.contains("id-006"));
}

/// Effective module configuration, rather than raw account declarations,
/// determines auth account names: disabled, wrong-backend, and manual-refresh
/// accounts must not be advertised, and a later configuration generation must
/// replace the inventory.
#[test]
fn effective_config_and_reconfigure_replace_google_auth_inventory() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let storage = Rc::new(storage::FsStorage::new(temp.path().join("storage")));
    let secrets = BTreeMap::from([
        (
            "email_client".to_owned(),
            tau_proto::SecretValue::new("native-email-client-id"),
        ),
        (
            "calendar_client".to_owned(),
            tau_proto::SecretValue::new("native-calendar-client-id"),
        ),
        (
            "manual_refresh".to_owned(),
            tau_proto::SecretValue::new("native-refresh-token"),
        ),
    ]);
    let config = tau_proto::json_to_cbor(&serde_json::json!({
        "email": {
            "enable": true,
            "accounts": [
                {
                    "id": "zeta",
                    "enable": true,
                    "from": "zeta@example.test",
                    "auth": {
                        "method": "oauth2",
                        "provider": "google",
                        "client_id_secret": "email_client"
                    }
                },
                {
                    "id": "manual-email",
                    "enable": true,
                    "from": "manual@example.test",
                    "auth": {
                        "method": "oauth2",
                        "provider": "google",
                        "client_id_secret": "email_client",
                        "refresh_token_secret": "manual_refresh"
                    }
                },
                {
                    "id": "disabled-email",
                    "enable": false,
                    "from": "disabled@example.test"
                },
                {
                    "id": "unsafe email",
                    "enable": true,
                    "from": "unsafe@example.test",
                    "auth": {
                        "method": "oauth2",
                        "provider": "google",
                        "client_id_secret": "email_client"
                    }
                }
            ]
        },
        "calendar": {
            "enable": true,
            "accounts": [
                {
                    "id": "alpha",
                    "enable": true,
                    "backend": {
                        "type": "google",
                        "client_id_secret": "calendar_client"
                    }
                },
                {
                    "id": "manual-calendar",
                    "enable": true,
                    "backend": {
                        "type": "google",
                        "client_id_secret": "calendar_client",
                        "refresh_token_secret": "manual_refresh"
                    }
                },
                {
                    "id": "feed",
                    "enable": true,
                    "backend": {
                        "type": "ics_feed",
                        "url": "https://example.test/calendar.ics"
                    }
                },
                {
                    "id": "unsafe calendar",
                    "enable": true,
                    "backend": {
                        "type": "google",
                        "client_id_secret": "calendar_client"
                    }
                }
            ]
        }
    }));
    let mut runtime = RuntimeState::default();
    runtime
        .configure(
            tau_proto::Configure {
                tool_prefix: Some(
                    tau_proto::ToolNamePrefix::parse("work").expect("valid tool prefix"),
                ),
                config,
                instance_name: tau_proto::ExtensionName::parse("work-pim")
                    .expect("test extension name must satisfy the identifier grammar"),
                state_dir: Some(temp.path().join("state")),
                secrets: secrets.clone(),
                settings_files: Default::default(),
            },
            storage.clone(),
        )
        .expect("effective config");
    let schema = runtime.action_schema();
    assert_eq!(
        action_arg(&schema, &[":email", "auth", "google", "start"], "account")
            .suggestions
            .iter()
            .map(|choice| choice.value.as_str())
            .collect::<Vec<_>>(),
        vec!["zeta"]
    );
    assert_eq!(
        action_arg(
            &schema,
            &[":calendar", "auth", "google", "start"],
            "account"
        )
        .suggestions
        .iter()
        .map(|choice| choice.value.as_str())
        .collect::<Vec<_>>(),
        vec!["alpha"]
    );
    let debug = format!("{schema:?}");
    assert!(!debug.contains("native-email-client-id"));
    assert!(!debug.contains("native-calendar-client-id"));
    assert!(!debug.contains("native-refresh-token"));

    runtime
        .configure(
            tau_proto::Configure {
                tool_prefix: Some(
                    tau_proto::ToolNamePrefix::parse("work").expect("valid tool prefix"),
                ),
                config: tau_proto::json_to_cbor(&serde_json::json!({
                    "email": {"enable": true, "accounts": []},
                    "calendar": {
                        "enable": true,
                        "accounts": [{
                            "id": "current",
                            "enable": true,
                            "backend": {
                                "type": "google",
                                "client_id_secret": "calendar_client"
                            }
                        }]
                    }
                })),
                instance_name: tau_proto::ExtensionName::parse("work-pim")
                    .expect("test extension name must satisfy the identifier grammar"),
                state_dir: Some(temp.path().join("state")),
                secrets,
                settings_files: Default::default(),
            },
            storage,
        )
        .expect("replacement config");
    let schema = runtime.action_schema();
    assert!(
        action_arg(&schema, &[":email", "auth", "google", "start"], "account")
            .suggestions
            .is_empty()
    );
    assert_eq!(
        action_arg(
            &schema,
            &[":calendar", "auth", "google", "start"],
            "account"
        )
        .suggestions[0]
            .value,
        "current"
    );
}

fn action_arg<'a>(
    schema: &'a ActionSchema,
    path: &[&str],
    arg_name: &str,
) -> &'a tau_proto::ActionArg {
    let mut command = schema
        .roots
        .iter()
        .find(|command| command.name == path[0])
        .expect("root exists");
    for segment in &path[1..] {
        command = command
            .children
            .iter()
            .find(|child| child.name == *segment)
            .expect("child exists");
    }
    command
        .args
        .iter()
        .find(|arg| arg.name == arg_name)
        .expect("argument exists")
}

/// Calendar Google auth intentionally remains device-flow based and its finish
/// action does not accept Gmail's pasted redirect URL argument.
#[test]
fn calendar_google_auth_schema_remains_device_flow_shape() {
    let schema = action_schema_with_accounts(&[], &[]);
    let start = schema
        .parse_line(":calendar auth google start google")
        .expect("calendar auth start parses");
    assert_eq!(start.action_id, "calendar.auth.google.start");
    assert_eq!(start.argv, vec!["google".to_owned()]);

    let finish = schema
        .parse_line(":calendar auth google finish google")
        .expect("calendar auth finish parses");
    assert_eq!(finish.action_id, "calendar.auth.google.finish");
    assert_eq!(finish.argv, vec!["google".to_owned()]);

    assert!(
        schema
            .parse_line(
                ":calendar auth google finish google http://127.0.0.1:54321/?state=s&code=c",
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

        let local_tool_name = invoke.tool_name.clone();
        assert!(
            runtime
                .dispatch_tool(invoke, &local_tool_name)
                .expect("unknown dispatch succeeds")
                .is_none()
        );
    }
}

/// Ensures tau-client startup preserves PIM's split email/calendar tool
/// registrations, prompt fragments, action schema publication, exact
/// subscriptions, and pre-`Ready` ordering.
#[test]
fn startup_registers_email_and_calendar_tools() {
    let writer = SharedWriter::default();
    let written = writer.clone();
    let state_root = tempfile::TempDir::new().expect("state root");
    let secrets = BTreeMap::from([
        (
            "email_client".to_owned(),
            tau_proto::SecretValue::new("email-client-id"),
        ),
        (
            "calendar_client".to_owned(),
            tau_proto::SecretValue::new("calendar-client-id"),
        ),
    ]);
    let configured = |config| {
        let mut configured = configure(tau_proto::json_to_cbor(&config), state_root.path());
        configured.tool_prefix =
            Some(tau_proto::ToolNamePrefix::parse("work").expect("valid tool prefix"));
        configured.secrets = secrets.clone();
        configured
    };
    let input = {
        let mut bytes = Vec::new();
        let mut input = HarnessOutputWriter::new(&mut bytes);
        input
            .write_message(&HarnessOutputMessage::Configure(configured(
                serde_json::json!({
                    "email": {
                        "enable": true,
                        "accounts": [{
                            "id": "email-initial",
                            "enable": true,
                            "from": "email-initial@example.test",
                            "auth": {
                                "method": "oauth2",
                                "provider": "google",
                                "client_id_secret": "email_client"
                            }
                        }]
                    },
                    "calendar": {
                        "enable": true,
                        "accounts": [{
                            "id": "calendar-initial",
                            "enable": true,
                            "backend": {
                                "type": "google",
                                "client_id_secret": "calendar_client"
                            }
                        }]
                    }
                }),
            )))
            .expect("initial configure");
        input
            .write_message(&HarnessOutputMessage::Configure(configured(
                serde_json::json!({
                    "email": {
                        "enable": true,
                        "accounts": [{
                            "id": "email-current",
                            "enable": true,
                            "from": "email-current@example.test",
                            "auth": {
                                "method": "oauth2",
                                "provider": "google",
                                "client_id_secret": "email_client"
                            }
                        }]
                    },
                    "calendar": {"enable": true, "accounts": []}
                }),
            )))
            .expect("replacement configure");
        input.flush().expect("flush configures");
        bytes
    };
    let state = RuntimeState {
        storage: Some(Rc::new(storage::FsStorage::new(
            state_root.path().join("pim-data"),
        ))),
        ..RuntimeState::default()
    };
    tau_client::TauExtensionRunner::new(PimExtension)
        .run(path_std_io::Cursor::new(input), writer, state)
        .expect("startup writes");

    let bytes = written.bytes();
    let mut reader = HarnessInputReader::new(bytes.as_slice());
    let mut tools = Vec::new();
    let mut prompt_tools = Vec::new();
    let mut per_tool_prompt_tools = Vec::new();
    let mut saw_subscription = false;
    let mut saw_ready = false;
    let mut action_schemas = Vec::new();
    while let Some(frame) = reader.read_message().expect("frame decodes") {
        match frame {
            HarnessInputMessage::Subscribe(subscribe) => {
                assert!(!saw_ready, "Subscribe should be emitted before Ready");
                saw_subscription = subscribe.live_selectors
                    == vec![
                        EventSelector::Exact(EventName::TOOL_STARTED),
                        EventSelector::Exact(EventName::ACTION_INVOKE),
                    ];
            }
            HarnessInputMessage::Emit(emit)
                if matches!(emit.event.as_ref(), Event::ToolRegistrationDeclared(_)) =>
            {
                assert!(!saw_ready, "tools should be registered before Ready");
                let Event::ToolRegistrationDeclared(register) = *emit.event else {
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
            HarnessInputMessage::Emit(emit)
                if matches!(emit.event.as_ref(), Event::ActionSchemaPublished(_)) =>
            {
                let Event::ActionSchemaPublished(published) = *emit.event else {
                    unreachable!();
                };
                action_schemas.push((saw_ready, published.schema));
            }
            HarnessInputMessage::Ready(_) => {
                saw_ready = true;
            }
            _ => {}
        }
    }

    assert!(saw_subscription);
    assert!(saw_ready);
    assert_eq!(action_schemas.len(), 2);
    assert!(!action_schemas[0].0, "initial schema precedes Ready");
    assert!(action_schemas[1].0, "replacement schema follows Ready");
    for (_, schema) in &action_schemas {
        assert_eq!(
            schema
                .roots
                .iter()
                .map(|root| root.name.as_str())
                .collect::<Vec<_>>(),
            vec![":email", ":calendar"],
            "tool prefixes must not rewrite action roots"
        );
    }
    assert_eq!(
        action_arg(
            &action_schemas[0].1,
            &[":email", "auth", "google", "start"],
            "account",
        )
        .suggestions
        .iter()
        .map(|choice| choice.value.as_str())
        .collect::<Vec<_>>(),
        vec!["email-initial"]
    );
    assert_eq!(
        action_arg(
            &action_schemas[0].1,
            &[":calendar", "auth", "google", "start"],
            "account",
        )
        .suggestions
        .iter()
        .map(|choice| choice.value.as_str())
        .collect::<Vec<_>>(),
        vec!["calendar-initial"]
    );
    assert_eq!(
        action_arg(
            &action_schemas[1].1,
            &[":email", "auth", "google", "start"],
            "account",
        )
        .suggestions
        .iter()
        .map(|choice| choice.value.as_str())
        .collect::<Vec<_>>(),
        vec!["email-current"]
    );
    assert!(
        action_arg(
            &action_schemas[1].1,
            &[":calendar", "auth", "google", "start"],
            "account",
        )
        .suggestions
        .is_empty()
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.as_str() == "work_email_list_folders")
    );
    assert!(tools.iter().any(|tool| tool.as_str() == "work_email_send"));
    assert!(
        tools
            .iter()
            .any(|tool| tool.as_str() == "work_calendar_list_calendars")
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.as_str() == "work_calendar_respond")
    );
    assert!(
        prompt_tools
            .iter()
            .any(|group| group.as_str() == "work_email")
    );
    assert!(
        prompt_tools
            .iter()
            .any(|group| group.as_str() == "work_calendar")
    );
    assert!(
        per_tool_prompt_tools
            .iter()
            .any(|tool| tool.as_str() == "work_email_read")
    );
    assert!(
        per_tool_prompt_tools
            .iter()
            .any(|tool| tool.as_str() == "work_calendar_get")
    );
    assert!(!tools.iter().any(|tool| tool.as_str() == email::TOOL_NAME));
    assert!(
        !tools
            .iter()
            .any(|tool| tool.as_str() == calendar::TOOL_NAME)
    );
    assert_eq!(tools.len(), 18);
}

/// Production tau-client dispatch maps a prefixed calendar wire name back to
/// the logical handler while preserving the wire name on progress and terminal
/// output.
#[test]
fn prefixed_calendar_invocation_uses_logical_dispatch_and_wire_output() {
    let state_root = tempfile::TempDir::new().expect("state root");
    let mut configure = configure(CborValue::Map(Vec::new()), state_root.path());
    configure.tool_prefix = Some(tau_proto::ToolNamePrefix::parse("work").expect("valid prefix"));
    let input = {
        let mut bytes = Vec::new();
        let mut writer = HarnessOutputWriter::new(&mut bytes);
        writer
            .write_message(&HarnessOutputMessage::Configure(configure))
            .expect("configure");
        writer
            .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
                tau_proto::ToolStarted {
                    call_id: tau_proto::ToolCallId::new("prefixed-calendar"),
                    tool_name: tau_proto::ToolName::new("work_calendar_list_calendars"),
                    arguments: CborValue::Map(Vec::new()),
                    agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                    originator: tau_proto::PromptOriginator::User,
                },
            )))
            .expect("invoke");
        writer.flush().expect("flush");
        bytes
    };
    let output = SharedWriter::default();
    let written = output.clone();
    let state = RuntimeState {
        storage: Some(Rc::new(storage::FsStorage::new(
            state_root.path().join("pim-data"),
        ))),
        ..RuntimeState::default()
    };
    tau_client::TauExtensionRunner::new(PimExtension)
        .run(path_std_io::Cursor::new(input), output, state)
        .expect("run PIM");

    let mut reader = HarnessInputReader::new(path_std_io::Cursor::new(written.bytes()));
    let mut registered = false;
    let mut progress = false;
    let mut terminal = false;
    while let Some(frame) = reader.read_message().expect("frame") {
        let HarnessInputMessage::Emit(emit) = frame else {
            continue;
        };
        match emit.event.as_ref() {
            Event::ToolRegistrationDeclared(register)
                if register.tool.name.as_str() == "work_calendar_list_calendars" =>
            {
                registered = true;
            }
            Event::ToolProgressReported(event) if event.call_id.as_str() == "prefixed-calendar" => {
                assert_eq!(event.tool_name.as_str(), "work_calendar_list_calendars");
                progress = true;
            }
            Event::ToolResultReported(event) if event.call_id.as_str() == "prefixed-calendar" => {
                assert_eq!(event.tool_name.as_str(), "work_calendar_list_calendars");
                terminal = true;
            }
            Event::ToolErrorReported(event) if event.call_id.as_str() == "prefixed-calendar" => {
                assert_eq!(event.tool_name.as_str(), "work_calendar_list_calendars");
                terminal = true;
            }
            _ => {}
        }
    }
    assert!(registered && progress && terminal);
}

/// Ensures the public `run` path installs extension-data storage before
/// configuration and keeps tau-client's live-only action dispatch behavior.
#[test]
fn public_run_installs_storage_and_skips_replayed_actions() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let (reader_stream, mut input_stream) = UnixStream::pair().expect("unix stream pair");
    let output = SharedWriter::default();
    let written = output.clone();
    let responder = spawn_storage_responder(
        written.clone(),
        input_stream.try_clone().expect("clone input stream"),
    );
    let run_thread =
        std::thread::spawn(move || run(reader_stream, output).map_err(|error| error.to_string()));
    {
        let mut writer = HarnessOutputWriter::new(&mut input_stream);
        writer
            .write_message(&HarnessOutputMessage::Configure(configure(
                CborValue::Map(vec![]),
                temp.path(),
            )))
            .expect("write configure");
        writer
            .write_message(&HarnessOutputMessage::deliver_replay(
                tau_proto::UnixMicros::new(1),
                unknown_action("replayed-action"),
            ))
            .expect("write replayed action");
        writer
            .write_message(&HarnessOutputMessage::deliver_live(
                tau_proto::UnixMicros::new(2),
                unknown_action("live-action"),
            ))
            .expect("write live action");
        writer.flush().expect("flush input");
    }
    drop(input_stream);

    responder.join().expect("storage responder");
    run_thread.join().expect("run thread").expect("public run");

    let output_bytes = written.bytes();
    let mut reader = HarnessInputReader::new(output_bytes.as_slice());
    let mut config_errors = Vec::new();
    let mut action_errors = Vec::new();
    while let Some(frame) = reader.read_message().expect("output frame decodes") {
        match frame {
            HarnessInputMessage::ConfigError(error) => config_errors.push(error.message),
            HarnessInputMessage::Emit(emit) => {
                if let Event::ActionError(error) = *emit.event {
                    action_errors.push(error.invocation_id);
                }
            }
            _ => {}
        }
    }

    assert_eq!(config_errors, Vec::<String>::new());
    assert_eq!(
        action_errors,
        vec![
            tau_proto::ActionInvocationId::parse("live-action")
                .expect("test identifier must be valid")
        ]
    );
}

fn unknown_action(invocation_id: &str) -> Event {
    Event::ActionInvoke(tau_proto::ActionInvoke {
        invocation_id: tau_proto::ActionInvocationId::parse(invocation_id)
            .expect("test identifier must be valid"),
        session_id: tau_proto::SessionId::parse("session-1")
            .expect("known-safe SessionId must be valid"),
        extension_name: tau_proto::ExtensionName::parse("tau-ext-pim")
            .expect("test extension name must satisfy the identifier grammar"),
        instance_id: tau_proto::ExtensionInstanceId::from(1),
        action_id: "pim.unknown".to_owned(),
        raw_line: "/pim unknown".to_owned(),
        argv: Vec::new(),
        arguments: CborValue::Map(Vec::new()),
    })
}

fn spawn_storage_responder(
    writer: SharedWriter,
    input_stream: UnixStream,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut input_writer = HarnessOutputWriter::new(input_stream);
        let mut responded = BTreeSet::new();
        let mut last_response = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mut made_progress = false;
            for request in extension_data_requests_from_bytes(writer.bytes()) {
                if !responded.insert(request.request_id.clone()) {
                    continue;
                }
                input_writer
                    .write_message(&extension_data_result_for_request(request))
                    .expect("write storage response");
                input_writer.flush().expect("flush storage response");
                last_response = Instant::now();
                made_progress = true;
            }
            if responded.is_empty() || made_progress {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for PIM storage requests to settle"
                );
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            if last_response.elapsed() >= Duration::from_millis(50) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    })
}

fn extension_data_requests_from_bytes(bytes: Vec<u8>) -> Vec<tau_proto::ExtensionDataRequest> {
    let mut reader = HarnessInputReader::new(bytes.as_slice());
    let mut requests = Vec::new();
    loop {
        match reader.read_message() {
            Ok(Some(HarnessInputMessage::ExtensionDataRequest(request))) => requests.push(request),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    requests
}

fn extension_data_result_for_request(
    request: tau_proto::ExtensionDataRequest,
) -> HarnessOutputMessage {
    let result = match request.op {
        tau_proto::ExtensionDataRequestOp::ReadFile { .. } => {
            tau_proto::ExtensionDataResultPayload::Error {
                kind: tau_proto::ExtensionDataErrorKind::NotFound,
                message: "missing".to_owned(),
            }
        }
        tau_proto::ExtensionDataRequestOp::WriteFile { .. } => {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::WriteFile,
            }
        }
        tau_proto::ExtensionDataRequestOp::CompareAndSwapFile { .. } => {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::CompareAndSwapFile,
            }
        }
        tau_proto::ExtensionDataRequestOp::CreateFile { .. } => {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::CreateFile,
            }
        }
        tau_proto::ExtensionDataRequestOp::AppendFile { .. } => {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::AppendFile,
            }
        }
        tau_proto::ExtensionDataRequestOp::DeleteFile { .. } => {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::DeleteFile,
            }
        }
        tau_proto::ExtensionDataRequestOp::RenameFile { .. } => {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::RenameFile,
            }
        }
        tau_proto::ExtensionDataRequestOp::ListFiles { .. } => {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::ListFiles {
                    entries: Vec::new(),
                },
            }
        }
    };
    HarnessOutputMessage::ExtensionDataResult(Box::new(tau_proto::ExtensionDataResult {
        request_id: request.request_id,
        result,
    }))
}
