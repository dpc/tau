use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::sync as path_std_sync;

use tau_client::{ExtensionBuilder, TauExtension, TauExtensionRunner};
use tau_proto::{
    Event, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter,
    SecretValue,
};
use tau_swarm_api::{Agent, AgentActivity, AgentNavigationMode, AgentWorkStatus};
use tau_swarm_client::Application;
use tau_swarm_client_api::v0::BlockerAnswerKind;
use tau_swarm_client_api::{AnswerBlockerRequest, AnswerBlockerResponse};
use tokio::sync as path_tokio_sync;

use super::*;
use crate::application::SwarmApplication;
use crate::projection::{ProjectionLimits, SessionProjection};
use crate::worker_health::WorkerHealth;

/// Minimal runner used to exercise the production Swarm tool handlers.
struct ToolTestExtension;

impl TauExtension for ToolTestExtension {
    type State = SwarmRuntime;

    fn name(&self) -> &'static str {
        "swarm-tool-test"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        super::register(builder);
    }
}

/// Ensures Tau Swarm exposes exactly the three task-family public names, with
/// no legacy aliases, and keeps them disabled until ordinary role policy grants
/// their shared group or exact names.
#[test]
fn swarm_tools_are_grouped_and_disabled_by_default() {
    for (expected_name, tool) in [
        ("task_info", task_info_spec()),
        ("task_update", update_spec()),
        ("task_blocker", blocker_spec()),
    ] {
        assert_eq!(tool.name.as_str(), expected_name);
        assert_eq!(
            tool.model_visible_name
                .as_ref()
                .map(tau_proto::ToolName::as_str),
            Some(expected_name)
        );
        assert!(!tool.enabled_by_default);
        let declaration = declaration(tool);
        assert_eq!(
            declaration
                .tool_group
                .as_ref()
                .map(|group| group.name.as_str()),
            Some(TOOL_GROUP_NAME)
        );
    }
    assert_eq!(TASK_INFO_TOOL_NAME, "task_info");
    assert_eq!(TASK_UPDATE_TOOL_NAME, "task_update");
    assert_eq!(TASK_BLOCKER_TOOL_NAME, "task_blocker");
    assert!(!["update", "blocker", "swarm_update"].contains(&TASK_UPDATE_TOOL_NAME));
}

/// The actual extension registration surface contains only the approved names;
/// retired `update` and `blocker` calls cannot route through hidden aliases.
#[test]
fn extension_registers_no_legacy_tool_aliases() {
    let mut input = Vec::new();
    HarnessOutputWriter::new(&mut input)
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            config: tau_proto::CborValue::Null,
            instance_name: tau_proto::ExtensionName::parse("swarm-tool-test")
                .expect("instance name"),
            tool_prefix: None,
            state_dir: None,
            secrets: BTreeMap::new(),
            settings_files: Default::default(),
        }))
        .expect("configure");
    let mut output = Vec::new();
    TauExtensionRunner::new(ToolTestExtension)
        .run(Cursor::new(input), &mut output, SwarmRuntime::new())
        .expect("tool runner");
    let mut names = std::iter::from_fn({
        let mut reader = HarnessInputReader::new(output.as_slice());
        move || reader.read_message().transpose()
    })
    .collect::<Result<Vec<_>, _>>()
    .expect("tool output")
    .into_iter()
    .filter_map(|frame| match frame {
        HarnessInputMessage::Emit(emit) => match *emit.event {
            Event::ToolRegistrationDeclared(declaration) => {
                Some(declaration.tool.name.as_str().to_owned())
            }
            _ => None,
        },
        _ => None,
    })
    .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["task_blocker", "task_info", "task_update"]);
}

/// Ensures instance prefixes qualify the Swarm tool and group policy names
/// emitted from the extension's actual declarations.
#[test]
fn swarm_tool_declarations_apply_instance_prefixes() {
    let configure = tau_proto::Configure {
        config: tau_proto::CborValue::Null,
        instance_name: tau_proto::ExtensionName::parse("std-swarm").expect("instance name"),
        tool_prefix: Some(tau_proto::ToolNamePrefix::parse("work").expect("prefix")),
        state_dir: None,
        secrets: BTreeMap::new(),
        settings_files: Default::default(),
    };
    let scope = tau_client::ToolNameScope::from_configure(&configure);
    for (expected_tool, expected_group, declaration) in [
        (
            "work_task_info",
            "work_swarm",
            declaration(task_info_spec()),
        ),
        (
            "work_task_blocker",
            "work_swarm",
            declaration(blocker_spec()),
        ),
        ("work_task_update", "work_swarm", declaration(update_spec())),
    ] {
        let declaration = scope
            .scope_registration(declaration)
            .expect("scope registration");
        assert_eq!(declaration.tool.name.as_str(), expected_tool);
        assert_eq!(
            declaration
                .tool_group
                .as_ref()
                .map(|group| group.name.as_str()),
            Some(expected_group)
        );
    }
}

/// Tool schema advertises strict nullable replacement semantics while leaving
/// authoritative byte and scalar validation to runtime.
#[test]
fn task_info_schema_is_strict_and_nullable() {
    let parameters = task_info_spec().parameters.expect("parameters");
    assert_eq!(
        parameters["required"],
        serde_json::json!(["task_id", "title"])
    );
    assert_eq!(parameters["additionalProperties"], false);
    assert_eq!(
        parameters["properties"]["description"]["type"],
        serde_json::json!(["string", "null"])
    );
    assert_eq!(parameters["properties"]["task_id"]["maxLength"], 128);
    assert_eq!(parameters["properties"]["title"]["maxLength"], 160);
    assert_eq!(parameters["properties"]["description"]["maxLength"], 16_384);
}

/// Task metadata canonicalization trims only the title, treats missing/null as
/// description clearing, and preserves opaque task and description whitespace.
#[test]
fn task_info_canonicalizes_exactly_the_approved_fields() {
    let canonical = canonicalize_task_info(TaskInfoArgs {
        task_id: " task ".into(),
        title: "\u{2003}Canonical title\u{2003}".into(),
        description: Some("\tline one\nline two ".into()),
    })
    .expect("valid metadata");
    assert_eq!(canonical.task_id.as_str(), " task ");
    assert_eq!(canonical.title.as_str(), "Canonical title");
    assert_eq!(
        canonical.description.as_ref().map(TaskDescription::as_str),
        Some("\tline one\nline two ")
    );

    let cleared = canonicalize_task_info(
        serde_json::from_value::<TaskInfoArgs>(serde_json::json!({
            "task_id": "task",
            "title": "title",
            "description": null
        }))
        .expect("null description"),
    )
    .expect("cleared metadata");
    assert_eq!(cleared.description, None);
    let omitted = canonicalize_task_info(
        serde_json::from_value::<TaskInfoArgs>(serde_json::json!({
            "task_id": "task",
            "title": "title"
        }))
        .expect("omitted description"),
    )
    .expect("omitted description clears");
    assert_eq!(omitted.description, None);
}

/// Runtime validation counts UTF-8 bytes, rejects U+2028/U+2029 and forbidden
/// controls, and does not accept an empty description as a clearing alias.
#[test]
fn task_info_enforces_byte_and_scalar_contract() {
    let valid = |task_id: String, title: String, description: Option<String>| {
        canonicalize_task_info(TaskInfoArgs {
            task_id,
            title,
            description,
        })
    };
    assert!(valid("é".repeat(64), "é".repeat(80), Some("é".repeat(8_192))).is_ok());
    assert!(valid("é".repeat(65), "title".into(), None).is_err());
    assert!(valid("task".into(), "é".repeat(81), None).is_err());
    assert!(valid("task".into(), "title".into(), Some("é".repeat(8_193))).is_err());
    assert!(valid("task".into(), "title".into(), Some(String::new())).is_err());
    for forbidden in ['\0', '\r', '\u{2028}', '\u{2029}'] {
        assert!(valid(format!("task{forbidden}"), "title".into(), None).is_err());
        assert!(valid("task".into(), format!("title{forbidden}"), None).is_err());
        assert!(
            valid(
                "task".into(),
                "title".into(),
                Some(format!("body{forbidden}"))
            )
            .is_err()
        );
    }
}

/// Any loaded agent granted `task_info` may replace any exact task ID, and
/// success returns the canonical installed value rather than the raw arguments.
#[test]
fn task_info_uses_agent_grant_not_task_ownership() {
    let mut state = configured_runtime();
    state
        .projection
        .blocking_lock()
        .upsert_agent(Agent {
            id: tau_swarm_api::AgentId::new("other"),
            name: "Other".into(),
            activity: AgentActivity::Waiting,
            navigation_mode: AgentNavigationMode::Active,
            watches: BTreeSet::new(),
            work_status: AgentWorkStatus::Unreported,
        })
        .expect("second invoking agent");
    let first = replace_task_info(
        &mut state,
        "agent",
        TaskInfoArgs {
            task_id: "task".into(),
            title: " First ".into(),
            description: Some("details".into()),
        },
    )
    .expect("first agent metadata");
    assert_eq!(
        first,
        serde_json::json!({
            "task_id":"task",
            "title":"First",
            "description":"details"
        })
    );
    let replacement = replace_task_info(
        &mut state,
        "other",
        TaskInfoArgs {
            task_id: "task".into(),
            title: "Replacement".into(),
            description: None,
        },
    )
    .expect("other agent replacement");
    assert_eq!(
        replacement,
        serde_json::json!({
            "task_id":"task",
            "title":"Replacement",
            "description":null
        })
    );
    assert_eq!(
        state
            .projection
            .blocking_lock()
            .snapshot()
            .snapshot
            .task_info
            .len(),
        1
    );
}

/// Tool-level entry-capacity failure leaves the canonical installed metadata,
/// revision, and live history unchanged, while replacement at the ceiling fits.
#[test]
fn task_info_tool_capacity_failure_is_transactional() {
    let mut state = configured_runtime();
    state
        .config
        .as_mut()
        .expect("config")
        .projection_limits
        .task_info_entries = 1;
    *state.projection.blocking_lock() = SessionProjection::new(ProjectionLimits {
        history_entries: 8,
        task_info_entries: 1,
        ..ProjectionLimits::unconfigured()
    });
    state
        .projection
        .blocking_lock()
        .upsert_agent(Agent {
            id: tau_swarm_api::AgentId::new("agent"),
            name: "Agent".into(),
            activity: AgentActivity::Waiting,
            navigation_mode: AgentNavigationMode::Active,
            watches: BTreeSet::new(),
            work_status: AgentWorkStatus::Unreported,
        })
        .expect("owner projection");
    replace_task_info(
        &mut state,
        "agent",
        TaskInfoArgs {
            task_id: "task".into(),
            title: "First".into(),
            description: None,
        },
    )
    .expect("first");
    replace_task_info(
        &mut state,
        "agent",
        TaskInfoArgs {
            task_id: "task".into(),
            title: "Replacement".into(),
            description: None,
        },
    )
    .expect("replacement at ceiling");
    let before = state.projection.blocking_lock().snapshot();
    assert!(
        replace_task_info(
            &mut state,
            "agent",
            TaskInfoArgs {
                task_id: "second".into(),
                title: "Second".into(),
                description: None,
            },
        )
        .is_err()
    );
    assert_eq!(state.projection.blocking_lock().snapshot(), before);
}

fn configured_runtime() -> SwarmRuntime {
    let peer_id = iroh::SecretKey::generate().public().to_string();
    let config: crate::config::ExtConfig = serde_json::from_value(serde_json::json!({
        "endpoint": {"peer_id": peer_id},
        "credential_id": "worker",
        "credential_secret": "swarm",
        "hostname": "host"
    }))
    .expect("config shape");
    let mut state = SwarmRuntime::new();
    state.config = Some(
        config
            .resolve(&BTreeMap::from([(
                "swarm".into(),
                SecretValue::new("secret"),
            )]))
            .expect("resolved config"),
    );
    state.replay_complete = true;
    state.worker_health = WorkerHealth::running();
    state
        .projection
        .blocking_lock()
        .upsert_agent(Agent {
            id: tau_swarm_api::AgentId::new("agent"),
            name: "Agent".into(),
            activity: AgentActivity::Waiting,
            navigation_mode: AgentNavigationMode::Active,
            watches: BTreeSet::new(),
            work_status: AgentWorkStatus::Unreported,
        })
        .expect("owner projection");
    state
}

/// Worker termination makes all three mutating tools fail before changing the
/// projection or blocker history, so no successful local state can outlive its
/// sole publisher.
#[test]
fn terminal_worker_rejects_mutations_without_changing_state() {
    let mut state = configured_runtime();
    let blocker = add_blocker(
        &mut state,
        "agent",
        "existing".into(),
        "description".into(),
        None,
        None,
    )
    .expect("blocker before retirement");
    let blocker_id = blocker
        .get("blocker_id")
        .and_then(serde_json::Value::as_str)
        .expect("blocker ID")
        .to_owned();
    drop(state.worker_health.terminal_guard());
    let before = state.projection.blocking_lock().snapshot();

    assert_eq!(
        add_blocker(
            &mut state,
            "agent",
            "blocked".into(),
            "description".into(),
            None,
            None,
        ),
        Err(
            "Tau Swarm owner is unavailable until successful replay has a live publication worker"
                .into()
        )
    );
    assert_eq!(
        cancel_blocker(&mut state, "agent", blocker_id, None),
        Err(
            "Tau Swarm owner is unavailable until successful replay has a live publication worker"
                .into()
        )
    );
    assert_eq!(
        add_update(
            &mut state,
            "agent",
            UpdateArgs {
                title: "task_update".into(),
                description: "description".into(),
                task_id: None,
            },
        ),
        Err(
            "Tau Swarm owner is unavailable until successful replay has a live publication worker"
                .into()
        )
    );
    assert_eq!(
        replace_task_info(
            &mut state,
            "agent",
            TaskInfoArgs {
                task_id: "task".into(),
                title: "Title".into(),
                description: None,
            },
        ),
        Err(
            "Tau Swarm owner is unavailable until successful replay has a live publication worker"
                .into()
        )
    );

    assert_eq!(state.blocker_history.lock().expect("history").len(), 1);
    assert_eq!(state.projection.blocking_lock().snapshot(), before);
}

/// A routed call received after worker death emits only an authoritative tool
/// error for all three mutating tools, never a successful terminal.
#[test]
fn terminal_worker_cannot_report_successful_tool_results() {
    for (name, arguments) in [
        (
            "task_blocker",
            serde_json::json!({
                "action": "add",
                "title": "blocked",
                "description": "description"
            }),
        ),
        (
            "task_update",
            serde_json::json!({
                "title": "task_update",
                "description": "description"
            }),
        ),
        (
            "task_info",
            serde_json::json!({
                "task_id": "task",
                "title": "Title"
            }),
        ),
    ] {
        let state = configured_runtime();
        drop(state.worker_health.terminal_guard());
        let mut input = Vec::new();
        let mut input_writer = HarnessOutputWriter::new(&mut input);
        input_writer
            .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
                config: tau_proto::CborValue::Null,
                instance_name: tau_proto::ExtensionName::parse("swarm-tool-test")
                    .expect("instance name"),
                tool_prefix: None,
                state_dir: None,
                secrets: BTreeMap::new(),
                settings_files: Default::default(),
            }))
            .expect("configure");
        input_writer
            .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
                tau_proto::ToolStarted {
                    invocation_policy: tau_proto::ToolInvocationPolicy::default(),
                    call_id: tau_proto::ToolCallId::new(format!("call-{name}")),
                    tool_name: tau_proto::ToolName::new(name),
                    arguments: tau_proto::json_to_cbor(&arguments),
                    agent_id: tau_proto::AgentId::parse("agent").expect("agent ID"),
                    originator: tau_proto::PromptOriginator::User,
                },
            )))
            .expect("tool invocation");
        let mut output = Vec::new();

        TauExtensionRunner::new(ToolTestExtension)
            .run(Cursor::new(input), &mut output, state)
            .expect("tool runner");

        let frames = std::iter::from_fn({
            let mut reader = HarnessInputReader::new(output.as_slice());
            move || reader.read_message().transpose()
        })
        .collect::<Result<Vec<_>, _>>()
        .expect("tool output");
        assert_eq!(
            frames
                .iter()
                .filter(|frame| matches!(
                    frame,
                    HarnessInputMessage::Emit(emit)
                        if matches!(emit.event.as_ref(), Event::ToolErrorReported(_))
                ))
                .count(),
            1,
            "{name} must fail exactly once"
        );
        assert!(
            !frames.iter().any(|frame| matches!(
                frame,
                HarnessInputMessage::Emit(emit)
                    if matches!(emit.event.as_ref(), Event::ToolResultReported(_))
            )),
            "{name} must not report success"
        );
    }
}

/// The tagged action enum rejects fields belonging to another action instead
/// of deferring an ambiguous option bag to runtime validation.
#[test]
fn blocker_actions_reject_cross_action_fields() {
    assert!(
        serde_json::from_value::<BlockerArgs>(serde_json::json!({
            "action": "list",
            "reason": "not valid for list"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<BlockerArgs>(serde_json::json!({
            "action": "add",
            "title": "title",
            "description": "description",
            "blocker_id": "wrong-kind"
        }))
        .is_err()
    );
}

/// Once remote answer delivery reserves an active blocker, local cancellation
/// cannot win a second lifecycle transition.
#[test]
fn cancellation_rejects_reserved_answer() {
    let mut state = configured_runtime();
    let publication = BlockerPublication {
        blocker_id: BlockerId::new("task_blocker"),
        revision: BlockerRevisionNumber(1),
        owner: tau_swarm_api::AgentId::new("agent"),
        title: "title".into(),
        description: "description".into(),
        recommended_answer: None,
        task_id: None,
        source_timestamp: Timestamp(1),
    };
    state
        .projection
        .blocking_lock()
        .add_blocker(publication)
        .expect("active blocker");
    state
        .blocker_history
        .lock()
        .expect("history")
        .push(BlockerRecord {
            blocker_id: BlockerId::new("task_blocker"),
            revision: BlockerRevisionNumber(1),
            owner: tau_swarm_api::AgentId::new("agent"),
            title: "title".into(),
            description: "description".into(),
            recommended_answer: None,
            task_id: None,
            state: BlockerState::Active,
            answer: None,
            answer_kind: None,
            reason: None,
            reserved_answer_bytes: 1,
        });
    assert_eq!(
        cancel_blocker(&mut state, "agent", "task_blocker".into(), None),
        Err("blocker answer is already pending".into())
    );
}

/// Add/cancel prospective history must preserve bytes already reserved by a
/// concurrently pending answer for the same owner.
#[test]
fn owner_history_budget_includes_pending_answer_reservations() {
    let record = BlockerRecord {
        blocker_id: BlockerId::new("task_blocker"),
        revision: BlockerRevisionNumber(1),
        owner: tau_swarm_api::AgentId::new("agent"),
        title: "title".into(),
        description: "description".into(),
        recommended_answer: None,
        task_id: None,
        state: BlockerState::Active,
        answer: None,
        answer_kind: None,
        reason: None,
        reserved_answer_bytes: 7,
    };
    let history = vec![record.clone()];
    let mut prospective = history.clone();
    prospective[0].state = BlockerState::Cancelled;
    prospective[0].reason = Some("reason".into());
    let encoded = serde_json::to_vec(&prospective)
        .expect("history encoding")
        .len();
    assert_eq!(
        owner_history_fits(&history, "agent", &prospective, encoded + 7),
        Ok(true)
    );
    assert_eq!(
        owner_history_fits(&history, "agent", &prospective, encoded + 6),
        Ok(false)
    );
}

/// Update tool admission frees capacity only after Swarm acknowledgement and
/// rejects overflow before publication.
#[test]
fn update_tool_enforces_and_releases_outbox_capacity() {
    let mut state = configured_runtime();
    state.config.as_mut().expect("config").update_limits.entries = 1;
    let first = add_update(
        &mut state,
        "agent",
        UpdateArgs {
            title: "first".into(),
            description: "description".into(),
            task_id: None,
        },
    )
    .expect("first update");
    assert!(
        add_update(
            &mut state,
            "agent",
            UpdateArgs {
                title: "second".into(),
                description: "description".into(),
                task_id: None,
            },
        )
        .is_err()
    );
    let id = first
        .get("update_id")
        .and_then(serde_json::Value::as_str)
        .expect("update ID");
    state
        .projection
        .blocking_lock()
        .acknowledge_update(&UpdateId::new(id));
    add_update(
        &mut state,
        "agent",
        UpdateArgs {
            title: "second".into(),
            description: "description".into(),
            task_id: None,
        },
    )
    .expect("capacity after acknowledgement");
}

/// Blocker tool state crosses the real application answer boundary, updates
/// list history after canonical acceptance, deduplicates retry, and rejects a
/// stale revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocker_tool_answer_lifecycle_and_deduplication() {
    let mut state = tokio::task::block_in_place(configured_runtime);
    let added = tokio::task::block_in_place(|| {
        add_blocker(
            &mut state,
            "agent",
            "title".into(),
            "description".into(),
            None,
            None,
        )
    })
    .expect("add blocker");
    let blocker_id = added
        .get("blocker_id")
        .and_then(serde_json::Value::as_str)
        .expect("blocker ID")
        .to_owned();
    let (_prompt_tx, _prompt_rx) = path_tokio_sync::mpsc::channel(2);
    let (blocker_tx, mut blockers) = path_tokio_sync::mpsc::channel(2);
    let application = path_std_sync::Arc::new(
        SwarmApplication::new(
            tau_swarm_api::SessionIdentity::new(
                tau_swarm_api::Hostname::new("host"),
                tau_swarm_api::SessionId::new("session"),
            ),
            path_std_sync::Arc::clone(&state.projection),
            path_std_sync::Arc::clone(&state.changed),
            _prompt_tx,
            blocker_tx,
        )
        .with_blocker_history(
            path_std_sync::Arc::clone(&state.blocker_history),
            BlockerHistoryLimits::default(),
        ),
    );
    let request = AnswerBlockerRequest {
        command_id: "command".into(),
        blocker_id: blocker_id.clone(),
        revision: 1,
        kind: BlockerAnswerKind::Custom,
        response: "answer".into(),
    };
    let answer = tokio::spawn({
        let application = path_std_sync::Arc::clone(&application);
        let request = request.clone();
        async move { application.answer_blocker(request).await }
    });
    blockers
        .recv()
        .await
        .expect("answer submission")
        .completion
        .send(Ok(()))
        .expect("canonical acceptance");
    assert_eq!(
        answer.await.expect("answer task"),
        Ok(AnswerBlockerResponse::Accepted)
    );
    let listed = tokio::task::block_in_place(|| list_blockers(&state, "agent"))
        .expect("list")
        .as_array()
        .expect("history array")
        .clone();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].get("state").and_then(serde_json::Value::as_str),
        Some("answered")
    );
    assert_eq!(
        application.answer_blocker(request.clone()).await,
        Ok(AnswerBlockerResponse::Accepted)
    );
    assert!(blockers.try_recv().is_err());
    let mut stale = request;
    stale.command_id = "stale".into();
    stale.revision = 2;
    assert!(matches!(
        application.answer_blocker(stale).await,
        Ok(AnswerBlockerResponse::Rejected(_))
    ));
    let second = tokio::task::block_in_place(|| {
        add_blocker(
            &mut state,
            "agent",
            "second".into(),
            "description".into(),
            None,
            None,
        )
    })
    .expect("second blocker");
    tokio::task::block_in_place(|| {
        state
            .projection
            .blocking_lock()
            .remove_agent(&tau_swarm_api::AgentId::new("agent"))
    })
    .expect("remove owner");
    assert!(matches!(
        application
            .answer_blocker(AnswerBlockerRequest {
                command_id: "missing-owner".into(),
                blocker_id: second
                    .get("blocker_id")
                    .and_then(serde_json::Value::as_str)
                    .expect("blocker ID")
                    .into(),
                revision: 1,
                kind: BlockerAnswerKind::Custom,
                response: "answer".into(),
            })
            .await,
        Ok(AnswerBlockerResponse::Rejected(_))
    ));
}
