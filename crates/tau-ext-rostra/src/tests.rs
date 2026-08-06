//! Deterministic contract tests for validation and hostile-content projection.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::Instant;
use std::{fs, thread};

use rostra_client::{Client, Database, ExternalEventId};
use rostra_client_db::social::EventPaginationCursor;
use rostra_core::event::PersonaTag;
use rostra_core::event::content_kind::PersonasTagsSelector;
use rostra_core::id::RostraIdSecretKey;
use tau_proto::{
    Event, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter,
    ToolCancelRequest,
};
use tokio::sync::Notify;

use super::*;
use crate::cursor::{Position, Timeline};
use crate::projection::{
    EXTERNAL_CLOSE, format_tags, sanitize_external, sanitize_line, truncate_utf8,
};
use crate::specs::{
    FOLLOW_TOOL, LIST_TOOL, POST_TOOL, PROFILE_TOOL, PROFILE_UPDATE_TOOL, REACT_TOOL, READ_TOOL,
    STATUS_TOOL, TOOL_GROUP, UNFOLLOW_TOOL, VOTE_TOOL, follow_spec, list_spec, post_spec,
    profile_spec, profile_update_spec, react_spec, read_spec, status_spec, unfollow_spec,
    vote_spec,
};
use crate::tools::write::{
    handle as handle_signed_tool_with_limit, parse_tags, pause_before_test_publication,
    validate_body,
};
use crate::tools::{ToolFailure, tool_error};

/// Thread-safe protocol writer used to observe asynchronous extension output.
#[derive(Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl SharedWriter {
    /// Return every complete or in-progress frame written so far.
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().expect("shared writer lock").clone()
    }
}

impl Write for SharedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("shared writer lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Run one signed tool with the production default post-like quota.
async fn handle_signed_tool(
    invoke: &tau_proto::ToolStarted,
    client: &Client,
    secret: RostraIdSecretKey,
    write_lock: Arc<AsyncMutex<()>>,
    publication_admitted: Arc<AtomicBool>,
) -> Result<String, crate::tools::ToolFailure> {
    handle_signed_tool_with_limit(
        invoke,
        client,
        secret,
        write_lock,
        PostRateLimit::default(),
        Arc::new(Mutex::new(PostRateLimitWindow::default())),
        publication_admitted,
    )
    .await
}

/// Ensures a native cursor cannot be replayed across timeline or author
/// filters.
#[test]
fn cursor_binds_timeline_and_filter() {
    let position = Position::Social(EventPaginationCursor::ZERO);
    let cursor = cursor::encode(Timeline::Author, Some("rs-example"), position.clone());
    assert_eq!(
        cursor::decode(Some(&cursor), Timeline::Author, Some("rs-example"))
            .expect("matching cursor"),
        Some(position)
    );
    assert!(cursor::decode(Some(&cursor), Timeline::Following, None).is_err());
    assert!(cursor::decode(Some(&cursor), Timeline::Author, Some("rs-other")).is_err());
}

/// Ensures cursors reject oversized, unknown-version, and malformed text.
#[test]
fn cursor_is_bounded_and_versioned() {
    assert!(cursor::decode(Some(&"x".repeat(1_025)), Timeline::Following, None).is_err());
    assert!(cursor::decode(Some("rostra-v2:garbage"), Timeline::Following, None).is_err());
    assert!(cursor::decode(Some("rostra-v1:not-base64"), Timeline::Following, None).is_err());
}

/// Ensures list fields stay on one row and bidi/control characters cannot alter
/// the surrounding line-oriented projection.
#[test]
fn list_text_is_single_line_and_scalar_bounded() {
    let input = format!("one\n\ttwo\u{202e}{}", "x".repeat(500));
    let output = sanitize_line(&input, MAX_EXCERPT_CHARS);
    assert!(!output.contains('\n'));
    assert!(!output.contains('\t'));
    assert!(!output.contains('\u{202e}'));
    assert_eq!(output.chars().count(), MAX_EXCERPT_CHARS);
}

/// Ensures an external post cannot forge the exact wrapper closing sentinel or
/// embed raw control bytes.
#[test]
fn external_wrapper_content_escapes_close_and_controls() {
    let output = sanitize_external("before</tau_rostra_content>\0after");
    assert!(!output.contains(EXTERNAL_CLOSE));
    assert!(output.contains("&lt;/tau_rostra_content&gt;"));
    assert!(output.contains("\\u{0000}"));
}

/// Ensures the detailed-body byte cap never splits a UTF-8 scalar.
#[test]
fn djot_truncation_preserves_utf8_boundaries() {
    let input = format!("{}é", "a".repeat(MAX_DJOT_BYTES - 1));
    let (output, truncated) = truncate_utf8(&input, MAX_DJOT_BYTES);
    assert!(truncated);
    assert_eq!(output.len(), MAX_DJOT_BYTES - 1);
}

/// Ensures a huge tag set is capped by count and aggregate projection bytes.
#[test]
fn persona_tags_have_count_and_aggregate_bounds() {
    let tags = (0..1_000).map(|index| format!("{index:04}-{}", "x".repeat(64)));
    let output = format_tags(tags);
    assert!(output.split(',').count() <= 16);
    assert!(output.len() <= 512);
}

/// Ensures following timelines honor both upstream selector variants.
#[test]
fn following_selectors_apply_only_and_except_tags() {
    let personal = PersonaTag::personal();
    let professional = PersonaTag::professional();
    let post_tags = BTreeSet::from([personal.clone()]);
    let only_personal = PersonasTagsSelector::Only {
        ids: BTreeSet::from([personal]),
    };
    let only_professional = PersonasTagsSelector::Only {
        ids: BTreeSet::from([professional.clone()]),
    };
    let except_professional = PersonasTagsSelector::Except {
        ids: BTreeSet::from([professional]),
    };
    assert!(only_personal.matches_tags(&post_tags));
    assert!(!only_professional.matches_tags(&post_tags));
    assert!(except_professional.matches_tags(&post_tags));
}

/// Ensures configuration accepts only a Tau secret reference and an optional
/// strict positive post-rate-limit object, never an identity or inline
/// mnemonic.
#[test]
fn config_schema_requires_mnemonic_secret_reference() {
    assert!(
        serde_json::from_value::<ExtConfig>(serde_json::json!({
            "identity_mnemonic_secret":"rostra_identity_mnemonic"
        }))
        .is_ok()
    );
    for invalid_limit in [
        serde_json::json!({"max_events":0,"window_seconds":1}),
        serde_json::json!({"max_events":1,"window_seconds":0}),
        serde_json::json!({"max_events":1,"window_seconds":1,"extra":true}),
        serde_json::Value::Null,
    ] {
        assert!(
            serde_json::from_value::<ExtConfig>(serde_json::json!({
                "identity_mnemonic_secret":"rostra_identity_mnemonic",
                "post_rate_limit": invalid_limit,
            }))
            .is_err()
        );
    }
    for excluded in [
        "identity",
        "public_mode",
        "secret",
        "secret_file",
        "mnemonic",
        "api_url",
    ] {
        let mut value = serde_json::json!({
            "identity_mnemonic_secret":"rostra_identity_mnemonic"
        });
        value[excluded] = serde_json::json!(false);
        assert!(serde_json::from_value::<ExtConfig>(value).is_err());
    }
    assert!(serde_json::from_value::<ExtConfig>(serde_json::json!({})).is_err());
}

/// Ensures configuration derives the identity from its Tau secret, leaves the
/// client unsigned until a signed call, and resets runtime quota state on
/// successful reconfiguration.
#[test]
fn mnemonic_configuration_derives_read_only_identity() {
    let runtime = RuntimeBuilder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let mut state = RostraState {
        client: None,
        identity_secret: None,
        runtime: Some(runtime),
        running: Arc::new(Mutex::new(HashMap::new())),
        permits: Arc::new(Semaphore::new(MAX_CONCURRENT_TOOLS)),
        write_lock: Arc::new(AsyncMutex::new(())),
        post_rate_limit: PostRateLimit::default(),
        post_rate_limit_window: Arc::new(Mutex::new(PostRateLimitWindow::default())),
        notifications: Arc::new(Mutex::new(notification_state::State::default())),
        notifications_wake: Arc::new(Notify::new()),
        notifications_task: None,
    };
    let mnemonic_secret = "rostra_identity_mnemonic";
    let configure_event = tau_proto::Configure {
        tool_prefix: None,
        config: tau_proto::CborValue::Map(Vec::new()),
        instance_name: tau_proto::ExtensionName::parse("std-rostra").expect("test extension name"),
        state_dir: Some(temporary.path().join("state")),
        secrets: BTreeMap::from([(
            mnemonic_secret.to_owned(),
            tau_proto::SecretValue::new(secret.to_string()),
        )]),
        settings_files: Default::default(),
    };
    configure(
        &mut state,
        &configure_event,
        ExtConfig {
            identity_mnemonic_secret: mnemonic_secret.to_owned(),
            post_rate_limit: PostRateLimit::default(),
        },
    )
    .expect("valid mnemonic configuration");
    let client = state.client.as_ref().expect("configured client");
    assert_eq!(client.rostra_id(), secret.id());
    assert_eq!(state.identity_secret, Some(secret));
    let current_head = state
        .runtime
        .as_ref()
        .expect("runtime")
        .block_on(client.db().get_self_current_head());
    assert_eq!(current_head, None);
    let limit: PostRateLimit = serde_json::from_value(serde_json::json!({
        "max_events": 1,
        "window_seconds": 3600,
    }))
    .expect("test limit");
    state
        .post_rate_limit_window
        .lock()
        .expect("post rate-limit state lock")
        .reserve(limit)
        .expect("fill runtime quota");
    let reconfigure_event = tau_proto::Configure {
        tool_prefix: None,
        config: tau_proto::CborValue::Map(Vec::new()),
        instance_name: tau_proto::ExtensionName::parse("std-rostra").expect("test extension name"),
        state_dir: Some(temporary.path().join("reconfigured-state")),
        secrets: BTreeMap::from([(
            mnemonic_secret.to_owned(),
            tau_proto::SecretValue::new(secret.to_string()),
        )]),
        settings_files: Default::default(),
    };
    configure(
        &mut state,
        &reconfigure_event,
        ExtConfig {
            identity_mnemonic_secret: mnemonic_secret.to_owned(),
            post_rate_limit: limit,
        },
    )
    .expect("successful reconfiguration");
    assert!(
        state
            .post_rate_limit_window
            .lock()
            .expect("post rate-limit state lock")
            .reserve(limit)
            .is_ok()
    );
}

/// Ensures all declarations retain exact names and strict object schemas.
#[test]
fn tool_declarations_match_approved_slice() {
    let specs = [
        status_spec(),
        list_spec(),
        read_spec(),
        profile_spec(),
        post_spec(),
        react_spec(),
        follow_spec(),
        unfollow_spec(),
        profile_update_spec(),
        vote_spec(),
    ];
    assert_eq!(
        specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>(),
        [
            STATUS_TOOL,
            LIST_TOOL,
            READ_TOOL,
            PROFILE_TOOL,
            POST_TOOL,
            REACT_TOOL,
            FOLLOW_TOOL,
            UNFOLLOW_TOOL,
            PROFILE_UPDATE_TOOL,
            VOTE_TOOL,
        ]
    );
    assert!(specs.iter().all(|spec| {
        spec.parameters
            .as_ref()
            .and_then(|schema| schema.get("additionalProperties"))
            == Some(&serde_json::Value::Bool(false))
    }));
}

/// Ensures the extension declares every standard Rostra tool in the one policy
/// group that grants the complete Rostra capability surface.
#[test]
fn standard_tool_registrations_share_rostra_group() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let (extension_input, harness_input) = UnixStream::pair().expect("input stream pair");
    let output = SharedWriter::default();
    let runner_output = output.clone();
    let runner = thread::spawn(move || {
        run(extension_input, runner_output).map_err(|error| error.to_string())
    });
    let mut writer = HarnessOutputWriter::new(harness_input);
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "identity_mnemonic_secret": "rostra_identity_mnemonic",
            })),
            instance_name: tau_proto::ExtensionName::parse("std-rostra").expect("extension name"),
            tool_prefix: None,
            state_dir: Some(temporary.path().join("state")),
            secrets: BTreeMap::from([(
                "rostra_identity_mnemonic".to_owned(),
                tau_proto::SecretValue::new(secret.to_string()),
            )]),
            settings_files: Default::default(),
        }))
        .expect("configure Rostra");
    writer.flush().expect("flush configuration");
    wait_for_output_event(&output, |_event| {
        output_events(&output)
            .iter()
            .filter(|event| matches!(event, Event::ToolRegistrationDeclared(_)))
            .count()
            == 11
    });

    let registrations = output_events(&output)
        .into_iter()
        .filter_map(|event| match event {
            Event::ToolRegistrationDeclared(registration) => Some(registration),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(registrations.len(), 11);
    assert_eq!(
        registrations
            .iter()
            .map(|registration| registration.tool.name.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            STATUS_TOOL,
            LIST_TOOL,
            READ_TOOL,
            PROFILE_TOOL,
            POST_TOOL,
            REACT_TOOL,
            FOLLOW_TOOL,
            UNFOLLOW_TOOL,
            PROFILE_UPDATE_TOOL,
            VOTE_TOOL,
            crate::specs::NOTIFICATIONS_TOOL,
        ])
    );
    assert!(registrations.iter().all(|registration| {
        registration
            .tool_group
            .as_ref()
            .is_some_and(|group| group.name.as_str() == TOOL_GROUP)
    }));

    drop(writer);
    runner
        .join()
        .expect("Rostra runner thread")
        .expect("Rostra runner succeeds");
}

/// Ensures private setup removes inherited group/world directory access.
#[cfg(unix)]
#[test]
fn state_directory_and_database_are_owner_private() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().expect("temporary directory");
    let directory = temporary.path().join("state");
    ensure_private_directory(&directory).expect("private state directory");
    let file = directory.join("rostra.redb");
    fs::write(&file, b"test").expect("test file");
    ensure_private_file(&file).expect("private database");
    assert_eq!(
        fs::metadata(directory)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(file).expect("metadata").permissions().mode() & 0o777,
        0o600
    );
}

/// Ensures database reopen works while lock, corruption, and identity mismatch
/// fail closed.
#[test]
fn database_lifecycle_fails_closed() {
    let runtime = RuntimeBuilder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let temporary = tempfile::tempdir().expect("temporary directory");
    let path = temporary.path().join("rostra.redb");
    let first_id = RostraIdSecretKey::generate().id();
    let second_id = RostraIdSecretKey::generate().id();

    runtime.block_on(async {
        let database = Database::open(&path, first_id).await.expect("new database");
        assert!(Database::open(&path, first_id).await.is_err());
        drop(database);
        drop(
            Database::open(&path, first_id)
                .await
                .expect("reopen database"),
        );
        assert!(Database::open(&path, second_id).await.is_err());
    });

    let corrupt = temporary.path().join("corrupt.redb");
    fs::write(&corrupt, b"not a redb database").expect("corrupt fixture");
    assert!(runtime.block_on(Database::open(corrupt, first_id)).is_err());
}

/// Ensures an unavailable Rostra database produces the typed pre-Ready
/// configuration failure rather than declarations or an unsafe ready state.
#[test]
fn locked_database_configuration_reports_storage_failure_before_ready() {
    let runtime = RuntimeBuilder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state_dir = temporary.path().join("state");
    let database_path = state_dir.join("rostra.redb");
    let secret = RostraIdSecretKey::generate();
    fs::create_dir_all(&state_dir).expect("state directory");
    let held_database = runtime
        .block_on(Database::open(database_path, secret.id()))
        .expect("hold database lock");
    let (extension_input, harness_input) = UnixStream::pair().expect("input stream pair");
    let output = SharedWriter::default();
    let runner_output = output.clone();
    let runner = thread::spawn(move || {
        run(extension_input, runner_output).map_err(|error| error.to_string())
    });
    let mut writer = HarnessOutputWriter::new(harness_input);
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "identity_mnemonic_secret": "rostra_identity_mnemonic",
            })),
            instance_name: tau_proto::ExtensionName::parse("std-rostra").expect("extension name"),
            tool_prefix: None,
            state_dir: Some(state_dir),
            secrets: BTreeMap::from([(
                "rostra_identity_mnemonic".to_owned(),
                tau_proto::SecretValue::new(secret.to_string()),
            )]),
            settings_files: Default::default(),
        }))
        .expect("configure Rostra");
    writer.flush().expect("flush configuration");

    let deadline = Instant::now() + Duration::from_secs(2);
    while !output_messages(&output).iter().any(|message| {
        matches!(
            message,
            HarnessInputMessage::ConfigError(error)
                if error.message.starts_with("storage_failure:")
        )
    }) {
        assert!(
            Instant::now() < deadline,
            "locked database must report ConfigError"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        !output_messages(&output)
            .iter()
            .any(|message| matches!(message, HarnessInputMessage::Ready(_))),
        "failed initialization must not announce readiness"
    );

    drop(writer);
    runner
        .join()
        .expect("Rostra runner thread")
        .expect("Rostra runner succeeds after config rejection");
    drop(held_database);
}

/// Ensures admission never queues a ninth retained database query and busy
/// reconfiguration fails its precondition.
#[test]
fn query_admission_is_capped_and_blocks_reconfiguration() {
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_TOOLS));
    let held = (0..MAX_CONCURRENT_TOOLS)
        .map(|_| Arc::clone(&permits).try_acquire_owned().expect("permit"))
        .collect::<Vec<_>>();
    assert!(Arc::clone(&permits).try_acquire_owned().is_err());
    assert!(!reconfiguration_allowed(&permits));
    drop(held);
    assert!(reconfiguration_allowed(&permits));
}

/// Ensures model-visible timeout does not falsely claim that retained blocking
/// work or its admission permit was cancelled.
#[test]
fn timeout_suppresses_terminal_while_retained_work_holds_permit() {
    let runtime = RuntimeBuilder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let permits = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&permits).try_acquire_owned().expect("permit");
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let retained = tokio::spawn(async move {
            let _permit = permit;
            let _ = release_rx.await;
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(1), async {
                std::future::pending::<()>().await;
            })
            .await
            .is_err()
        );
        assert_eq!(permits.available_permits(), 0);
        release_tx.send(()).expect("release retained query");
        retained.await.expect("retained query");
        assert_eq!(permits.available_permits(), 1);
    });
}

/// Ensures all timeline headers use their approved lowercase schema spelling.
#[test]
fn timeline_headers_are_lowercase() {
    assert_eq!(Timeline::Following.as_str(), "following");
    assert_eq!(Timeline::Network.as_str(), "network");
    assert_eq!(Timeline::Author.as_str(), "author");
}

/// Ensures bounded runtime shutdown returns even with retained async work.
#[test]
fn runtime_shutdown_is_bounded() {
    let runtime = RuntimeBuilder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.spawn(std::future::pending::<()>());
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        runtime.shutdown_timeout(Duration::from_millis(10));
        done_tx.send(()).expect("shutdown result");
    });
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("bounded shutdown");
}

/// Construct a minimal authenticated tool invocation.
fn signed_invoke(name: &str, arguments: serde_json::Value) -> tau_proto::ToolStarted {
    tau_proto::ToolStarted {
        call_id: format!("call-{name}").into(),
        tool_name: tau_proto::ToolName::new(name),
        arguments: tau_proto::json_to_cbor(&arguments),
        agent_id: tau_proto::AgentId::parse("agent").expect("test agent"),
        originator: tau_proto::PromptOriginator::User,
    }
}

/// Build a fresh write-boundary marker for direct signed-tool tests.
fn write_boundary() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// Decode all complete extension output frames currently captured by a writer.
fn output_events(output: &SharedWriter) -> Vec<Event> {
    let bytes = output.bytes();
    let mut reader = HarnessInputReader::new(bytes.as_slice());
    let mut events = Vec::new();
    while let Ok(Some(frame)) = reader.read_message() {
        if let HarnessInputMessage::Emit(emit) = frame {
            events.push(*emit.event);
        }
    }
    events
}

/// Decode every complete protocol frame currently captured by a writer.
fn output_messages(output: &SharedWriter) -> Vec<HarnessInputMessage> {
    let bytes = output.bytes();
    let mut reader = HarnessInputReader::new(bytes.as_slice());
    let mut messages = Vec::new();
    while let Ok(Some(frame)) = reader.read_message() {
        messages.push(frame);
    }
    messages
}

/// Wait for one expected extension event while its protocol input remains open.
fn wait_for_output_event(output: &SharedWriter, predicate: impl Fn(&Event) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if output_events(output).iter().any(&predicate) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for Rostra protocol output"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// Ensures a deadline or `ToolCancelRequest` after the admitted-publication
/// boundary reports exactly one early terminal, retains the write to
/// completion, and never emits a late result or retries it.
#[test]
fn signed_write_timeout_and_cancellation_retain_the_committing_lane() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let state_dir = temporary.path().join("state");
    let (extension_input, harness_input) = UnixStream::pair().expect("input stream pair");
    let output = SharedWriter::default();
    let runner_output = output.clone();
    let runner = thread::spawn(move || {
        run(extension_input, runner_output).map_err(|error| error.to_string())
    });
    let mut writer = HarnessOutputWriter::new(harness_input);
    let configure = tau_proto::Configure {
        config: tau_proto::json_to_cbor(&serde_json::json!({
            "identity_mnemonic_secret": "rostra_identity_mnemonic",
            "post_rate_limit": {"max_events": 2, "window_seconds": 3600},
        })),
        instance_name: tau_proto::ExtensionName::parse("std-rostra").expect("extension name"),
        tool_prefix: None,
        state_dir: Some(state_dir),
        secrets: BTreeMap::from([(
            "rostra_identity_mnemonic".to_owned(),
            tau_proto::SecretValue::new(secret.to_string()),
        )]),
        settings_files: Default::default(),
    };
    writer
        .write_message(&HarnessOutputMessage::Configure(configure))
        .expect("configure Rostra");

    let mut timeout_invoke = signed_invoke(
        "rostra_post",
        serde_json::json!({"body":"timeout after publication admission"}),
    );
    timeout_invoke.call_id = tau_proto::ToolCallId::new("timeout-after-admission");
    let (timeout_entered, timeout_release, timeout_committed) =
        pause_before_test_publication(timeout_invoke.call_id.clone());
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
            timeout_invoke.clone(),
        )))
        .expect("start timeout post");
    writer.flush().expect("flush timeout post");
    timeout_entered
        .recv_timeout(Duration::from_secs(2))
        .expect("timeout post reaches publication-admission boundary");
    wait_for_output_event(&output, |event| {
        matches!(
            event,
            Event::ToolErrorReported(error) if error.call_id == timeout_invoke.call_id
        )
    });
    timeout_release
        .send(())
        .expect("release timeout publication");
    timeout_committed
        .recv_timeout(Duration::from_secs(2))
        .expect("timeout publication commits exactly once");

    let mut cancelled_invoke = signed_invoke(
        "rostra_post",
        serde_json::json!({"body":"cancel after publication admission"}),
    );
    cancelled_invoke.call_id = tau_proto::ToolCallId::new("cancel-after-admission");
    let (cancelled_entered, cancelled_release, cancelled_committed) =
        pause_before_test_publication(cancelled_invoke.call_id.clone());
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
            cancelled_invoke.clone(),
        )))
        .expect("start cancelled post");
    writer.flush().expect("flush cancelled post");
    cancelled_entered
        .recv_timeout(Duration::from_secs(2))
        .expect("cancelled post reaches publication-admission boundary");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolCancelRequest(
            ToolCancelRequest {
                target_call_id: cancelled_invoke.call_id.clone(),
            },
        )))
        .expect("cancel signed post");
    writer.flush().expect("flush cancellation");
    wait_for_output_event(&output, |event| {
        matches!(
            event,
            Event::ToolCancelledReported(cancelled) if cancelled.call_id == cancelled_invoke.call_id
        )
    });
    cancelled_release
        .send(())
        .expect("release cancelled publication");
    cancelled_committed
        .recv_timeout(Duration::from_secs(2))
        .expect("cancelled publication commits exactly once");
    let mut limited_invoke = signed_invoke(
        "rostra_post",
        serde_json::json!({"body":"quota after uncertain writes"}),
    );
    limited_invoke.call_id = tau_proto::ToolCallId::new("rate-limited-after-uncertain-write");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
            limited_invoke.clone(),
        )))
        .expect("start post after uncertain writes");
    writer.flush().expect("flush limited post");
    wait_for_output_event(&output, |event| {
        matches!(
            event,
            Event::ToolErrorReported(error) if error.call_id == limited_invoke.call_id
        )
    });
    thread::sleep(Duration::from_millis(100));

    drop(writer);
    runner
        .join()
        .expect("Rostra runner thread")
        .expect("Rostra runner");

    let events = output_events(&output);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::ToolErrorReported(error) if error.call_id == timeout_invoke.call_id
            ))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::ToolCancelledReported(cancelled)
                    if cancelled.call_id == cancelled_invoke.call_id
            ))
            .count(),
        1
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::ToolResultReported(result)
            if result.call_id == timeout_invoke.call_id || result.call_id == cancelled_invoke.call_id
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::ToolErrorReported(error) if error.call_id == cancelled_invoke.call_id
    )));
    let Some(Event::ToolErrorReported(rate_limited)) = events.iter().find(|event| {
        matches!(
            event,
            Event::ToolErrorReported(error) if error.call_id == limited_invoke.call_id
        )
    }) else {
        panic!("post after retained uncertain writes must be rate limited");
    };
    assert!(rate_limited.message.starts_with("rate_limited:"));
    let details = rate_limited.details.as_ref().expect("rate-limit details");
    assert_eq!(
        tau_proto::cbor_text_field(details, "category").as_deref(),
        Some("rate_limited")
    );
    let retry_after =
        tau_proto::cbor_int_field(details, "retry_after_seconds").expect("retry-after integer");
    assert!(0 < retry_after && retry_after <= 3_600);
}

/// Keeps outbound post source below the approved local content bound.
#[test]
fn post_body_is_nonempty_and_byte_bounded() {
    assert!(validate_body("").is_err());
    assert!(validate_body(&"x".repeat(MAX_DJOT_BYTES)).is_ok());
    assert!(validate_body(&"x".repeat(MAX_DJOT_BYTES + 1)).is_err());
}

/// Rejects invalid or excess persona tags instead of silently dropping them.
#[test]
fn persona_tags_are_strict_and_bounded() {
    assert!(parse_tags(vec!["TAG".to_owned()]).is_ok());
    assert!(parse_tags(vec!["".to_owned()]).is_err());
    assert!(parse_tags(vec!["tag".to_owned(); 17]).is_err());
}

/// Exercises every approved signed operation against a real local Rostra
/// database and verifies their machine-readable local acknowledgements.
#[tokio::test(flavor = "multi_thread")]
async fn signed_tools_store_each_approved_operation_locally() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let identity = secret.id();
    let database = Database::open(temporary.path().join("rostra.redb"), identity)
        .await
        .expect("database");
    let client = Client::builder(identity)
        .start_background_tasks(false)
        .db(database)
        .public_mode(false)
        .build()
        .await
        .expect("read-only client");
    let write_lock = Arc::new(AsyncMutex::new(()));
    let post = handle_signed_tool(
        &signed_invoke("rostra_post", serde_json::json!({"body":"hello"})),
        &client,
        secret,
        Arc::clone(&write_lock),
        write_boundary(),
    )
    .await
    .expect("post");
    let post: serde_json::Value = serde_json::from_str(&post).expect("post result JSON");
    assert_eq!(post["operation"], "post");
    assert_eq!(post["local_state"], "stored");
    let post_id = post["event_id"].as_str().expect("post ID").to_owned();
    let post_id: ExternalEventId = post_id.parse().expect("valid post ID");
    let stored_post = client
        .db()
        .get_social_post(post_id.event_id())
        .await
        .expect("stored post");
    assert_eq!(stored_post.content.reaction, None);
    let post_id = post_id.to_string();

    let reply = handle_signed_tool(
        &signed_invoke(
            "rostra_post",
            serde_json::json!({"body":"a text reply","reply_to":post_id}),
        ),
        &client,
        secret,
        Arc::clone(&write_lock),
        write_boundary(),
    )
    .await
    .expect("reply");
    let reply: serde_json::Value = serde_json::from_str(&reply).expect("reply result");
    assert_eq!(reply["operation"], "reply");
    let reply_id: ExternalEventId = reply["event_id"]
        .as_str()
        .expect("reply ID")
        .parse()
        .expect("valid reply ID");
    assert_eq!(
        client
            .db()
            .get_social_post(reply_id.event_id())
            .await
            .expect("stored reply")
            .reply_to
            .map(|id| id.to_string()),
        Some(post_id.clone())
    );

    let heads_before_invalid = client.db().get_heads_self().await;
    for (name, arguments) in [
        (
            "rostra_post",
            serde_json::json!({"body":"👍","reply_to":post_id}),
        ),
        (
            "rostra_react",
            serde_json::json!({"post_id":post_id,"reaction":"plain text"}),
        ),
        (
            "rostra_react",
            serde_json::json!({"post_id":post_id,"reaction":"👍👎"}),
        ),
    ] {
        assert!(
            handle_signed_tool(
                &signed_invoke(name, arguments),
                &client,
                secret,
                Arc::clone(&write_lock),
                write_boundary(),
            )
            .await
            .is_err()
        );
    }
    assert_eq!(client.db().get_heads_self().await, heads_before_invalid);

    let reaction = handle_signed_tool(
        &signed_invoke(
            "rostra_react",
            serde_json::json!({"post_id":post_id,"reaction":"👍"}),
        ),
        &client,
        secret,
        Arc::clone(&write_lock),
        write_boundary(),
    )
    .await
    .expect("reaction");
    let reaction: serde_json::Value = serde_json::from_str(&reaction).expect("reaction result");
    assert_eq!(reaction["operation"], "reaction");
    let reaction_id: ExternalEventId = reaction["event_id"]
        .as_str()
        .expect("reaction ID")
        .parse()
        .expect("valid reaction ID");
    let stored_reaction = client
        .db()
        .get_social_post(reaction_id.event_id())
        .await
        .expect("stored reaction");
    assert_eq!(stored_reaction.content.djot_content, None);
    assert_eq!(stored_reaction.content.reaction.as_deref(), Some("👍"));

    let followee = RostraIdSecretKey::generate().id();
    let follow = handle_signed_tool(
        &signed_invoke(
            "rostra_follow",
            serde_json::json!({"identity":followee.to_string()}),
        ),
        &client,
        secret,
        Arc::clone(&write_lock),
        write_boundary(),
    )
    .await
    .expect("follow");
    let follow: serde_json::Value = serde_json::from_str(&follow).expect("follow result");
    assert_eq!(follow["operation"], "follow");
    assert_eq!(
        client.db().get_followees(identity).await,
        vec![(followee, PersonasTagsSelector::default())]
    );

    // Upstream singleton ordering uses timestamp then event ID. Advance the
    // timestamp so this state transition is deterministic rather than tied to
    // the random event-ID tiebreaker.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let unfollow = handle_signed_tool(
        &signed_invoke(
            "rostra_unfollow",
            serde_json::json!({"identity":followee.to_string()}),
        ),
        &client,
        secret,
        Arc::clone(&write_lock),
        write_boundary(),
    )
    .await
    .expect("unfollow");
    let unfollow: serde_json::Value = serde_json::from_str(&unfollow).expect("unfollow result");
    assert_eq!(unfollow["operation"], "unfollow");
    assert!(client.db().get_followees(identity).await.is_empty());

    let profile = handle_signed_tool(
        &signed_invoke(
            "rostra_update_profile",
            serde_json::json!({"display_name":"Tau","bio":"A test identity."}),
        ),
        &client,
        secret,
        Arc::clone(&write_lock),
        write_boundary(),
    )
    .await
    .expect("profile update");
    let profile: serde_json::Value = serde_json::from_str(&profile).expect("profile result");
    assert_eq!(profile["operation"], "profile_update");
    let stored_profile = client
        .db()
        .get_social_profile(identity)
        .await
        .expect("stored profile");
    assert_eq!(stored_profile.display_name, "Tau");
    assert_eq!(stored_profile.bio, "A test identity.");
    assert_eq!(stored_profile.avatar, None);

    let vote = handle_signed_tool(
        &signed_invoke(
            "rostra_vote",
            serde_json::json!({"post_id":post_id,"vote":"up"}),
        ),
        &client,
        secret,
        Arc::clone(&write_lock),
        write_boundary(),
    )
    .await
    .expect("vote");
    let vote: serde_json::Value = serde_json::from_str(&vote).expect("vote result");
    assert_eq!(vote["operation"], "vote");
    assert_eq!(
        client
            .db()
            .get_social_vote(identity, post_id.parse().expect("post ID"))
            .await,
        Some(Some(true))
    );
}

/// Ensures rejected signed input cannot activate the client or create a node
/// announcement before the requested operation validates.
#[tokio::test(flavor = "multi_thread")]
async fn invalid_signed_input_does_not_activate_or_store() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let identity = secret.id();
    let database = Database::open(temporary.path().join("rostra.redb"), identity)
        .await
        .expect("database");
    let client = Client::builder(identity)
        .start_background_tasks(false)
        .db(database)
        .public_mode(false)
        .build()
        .await
        .expect("read-only client");
    let result = handle_signed_tool(
        &signed_invoke("rostra_post", serde_json::json!({"body":""})),
        &client,
        secret,
        Arc::new(AsyncMutex::new(())),
        write_boundary(),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(client.db().get_self_current_head().await, None);
}

/// Ensures posts, replies, and reactions consume one shared runtime quota while
/// follow, profile-update, and vote mutations remain available.
#[tokio::test(flavor = "multi_thread")]
async fn post_rate_limit_covers_only_post_like_writes() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let identity = secret.id();
    let database = Database::open(temporary.path().join("rostra.redb"), identity)
        .await
        .expect("database");
    let client = Client::builder(identity)
        .start_background_tasks(false)
        .db(database)
        .public_mode(false)
        .build()
        .await
        .expect("read-only client");
    let write_lock = Arc::new(AsyncMutex::new(()));
    let limit: PostRateLimit = serde_json::from_value(serde_json::json!({
        "max_events": 3,
        "window_seconds": 3600,
    }))
    .expect("test limit");
    let window = Arc::new(Mutex::new(PostRateLimitWindow::default()));
    assert!(
        handle_signed_tool_with_limit(
            &signed_invoke("rostra_post", serde_json::json!({"body":""})),
            &client,
            secret,
            Arc::clone(&write_lock),
            limit,
            Arc::clone(&window),
            write_boundary(),
        )
        .await
        .is_err()
    );
    let post = handle_signed_tool_with_limit(
        &signed_invoke("rostra_post", serde_json::json!({"body":"parent"})),
        &client,
        secret,
        Arc::clone(&write_lock),
        limit,
        Arc::clone(&window),
        write_boundary(),
    )
    .await
    .expect("post");
    let post_id =
        serde_json::from_str::<serde_json::Value>(&post).expect("post result")["event_id"]
            .as_str()
            .expect("post ID")
            .to_owned();

    for (name, arguments) in [
        (
            "rostra_follow",
            serde_json::json!({"identity":RostraIdSecretKey::generate().id().to_string()}),
        ),
        (
            "rostra_update_profile",
            serde_json::json!({"display_name":"Tau","bio":"quota test"}),
        ),
        (
            "rostra_unfollow",
            serde_json::json!({"identity":RostraIdSecretKey::generate().id().to_string()}),
        ),
        (
            "rostra_vote",
            serde_json::json!({"post_id":post_id,"vote":"up"}),
        ),
    ] {
        handle_signed_tool_with_limit(
            &signed_invoke(name, arguments),
            &client,
            secret,
            Arc::clone(&write_lock),
            limit,
            Arc::clone(&window),
            write_boundary(),
        )
        .await
        .expect("excluded mutation");
    }
    handle_signed_tool_with_limit(
        &signed_invoke(
            "rostra_post",
            serde_json::json!({"body":"reply","reply_to":post_id}),
        ),
        &client,
        secret,
        Arc::clone(&write_lock),
        limit,
        Arc::clone(&window),
        write_boundary(),
    )
    .await
    .expect("reply");
    handle_signed_tool_with_limit(
        &signed_invoke(
            "rostra_react",
            serde_json::json!({"post_id":post_id,"reaction":"👍"}),
        ),
        &client,
        secret,
        Arc::clone(&write_lock),
        limit,
        Arc::clone(&window),
        write_boundary(),
    )
    .await
    .expect("reaction");
    let error = handle_signed_tool_with_limit(
        &signed_invoke("rostra_post", serde_json::json!({"body":"over quota"})),
        &client,
        secret,
        write_lock,
        limit,
        window,
        write_boundary(),
    )
    .await
    .expect_err("fourth post-like write");
    let Event::ToolError(error) = crate::tools::tool_error(
        &signed_invoke("rostra_post", serde_json::json!({"body":"over quota"})),
        error,
    ) else {
        panic!("rate limit must produce a tool error");
    };
    assert!(error.message.starts_with("rate_limited:"));
    let details = error.details.expect("structured rate-limit details");
    let tau_proto::CborValue::Map(entries) = &details else {
        panic!("rate-limit details must be a map");
    };
    assert_eq!(entries.len(), 2);
    assert_eq!(
        tau_proto::cbor_text_field(&details, "category").as_deref(),
        Some("rate_limited")
    );
    let retry_after =
        tau_proto::cbor_int_field(&details, "retry_after_seconds").expect("retry-after integer");
    assert!(0 < retry_after && retry_after <= 3_600);
}

/// Ensures rate-limit terminals expose exactly the fixed structured details
/// shape instead of requiring model code to parse the bounded prose.
#[test]
fn rate_limit_error_has_exact_structured_details() {
    let invoke = signed_invoke("rostra_post", serde_json::json!({"body":"limited"}));
    let Event::ToolError(error) = tool_error(&invoke, ToolFailure::rate_limited(17)) else {
        panic!("rate limit must produce a tool error");
    };
    assert_eq!(
        error.message,
        "rate_limited: post rate limit reached; retry after 17 seconds"
    );
    let tau_proto::CborValue::Map(entries) = error.details.expect("rate-limit details") else {
        panic!("rate-limit details must be a map");
    };
    assert_eq!(entries.len(), 2);
    let details = tau_proto::CborValue::Map(entries);
    assert_eq!(
        tau_proto::cbor_text_field(&details, "category").as_deref(),
        Some("rate_limited")
    );
    assert_eq!(
        tau_proto::cbor_int_field(&details, "retry_after_seconds"),
        Some(17)
    );
}

/// Ensures concurrent callers share the serialized runtime reservation and only
/// one caller can claim a final quota slot.
#[tokio::test(flavor = "multi_thread")]
async fn post_rate_limit_serializes_the_final_slot() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let identity = secret.id();
    let database = Database::open(temporary.path().join("rostra.redb"), identity)
        .await
        .expect("database");
    let client = Client::builder(identity)
        .start_background_tasks(false)
        .db(database)
        .public_mode(false)
        .build()
        .await
        .expect("read-only client");
    let write_lock = Arc::new(AsyncMutex::new(()));
    let limit: PostRateLimit = serde_json::from_value(serde_json::json!({
        "max_events": 1,
        "window_seconds": 3600,
    }))
    .expect("test limit");
    let window = Arc::new(Mutex::new(PostRateLimitWindow::default()));
    let first_invoke = signed_invoke("rostra_post", serde_json::json!({"body":"first"}));
    let second_invoke = signed_invoke("rostra_post", serde_json::json!({"body":"second"}));
    let first = handle_signed_tool_with_limit(
        &first_invoke,
        &client,
        secret,
        Arc::clone(&write_lock),
        limit,
        Arc::clone(&window),
        write_boundary(),
    );
    let second = handle_signed_tool_with_limit(
        &second_invoke,
        &client,
        secret,
        write_lock,
        limit,
        window,
        write_boundary(),
    );
    let (first, second) = tokio::join!(first, second);
    assert_eq!(
        [first, second]
            .iter()
            .filter(|result| result.is_ok())
            .count(),
        1
    );
}

/// Ensures concurrent signed calls serialize into one local head chain rather
/// than independently selecting the same parent.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_signed_posts_share_one_head_chain() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let identity = secret.id();
    let database = Database::open(temporary.path().join("rostra.redb"), identity)
        .await
        .expect("database");
    let client = Client::builder(identity)
        .start_background_tasks(false)
        .db(database)
        .public_mode(false)
        .build()
        .await
        .expect("read-only client");
    let write_lock = Arc::new(AsyncMutex::new(()));
    let first_invoke = signed_invoke("rostra_post", serde_json::json!({"body":"first"}));
    let second_invoke = signed_invoke("rostra_post", serde_json::json!({"body":"second"}));
    let first = handle_signed_tool(
        &first_invoke,
        &client,
        secret,
        Arc::clone(&write_lock),
        write_boundary(),
    );
    let second = handle_signed_tool(
        &second_invoke,
        &client,
        secret,
        Arc::clone(&write_lock),
        write_boundary(),
    );
    let (first, second) = tokio::join!(first, second);
    assert!(first.is_ok());
    assert!(second.is_ok());
    assert_eq!(client.db().get_heads_self().await.len(), 1);
}

/// Ensures a deadline before the write lane is acquired neither activates nor
/// stores an operation; a later caller remains a new, independent intent.
#[tokio::test(flavor = "multi_thread")]
async fn timed_out_waiting_write_has_no_implicit_retry() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let identity = secret.id();
    let database = Database::open(temporary.path().join("rostra.redb"), identity)
        .await
        .expect("database");
    let client = Client::builder(identity)
        .start_background_tasks(false)
        .db(database)
        .public_mode(false)
        .build()
        .await
        .expect("read-only client");
    let write_lock = Arc::new(AsyncMutex::new(()));
    let guard = write_lock.lock().await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1),
            handle_signed_tool(
                &signed_invoke("rostra_post", serde_json::json!({"body":"timed out"})),
                &client,
                secret,
                Arc::clone(&write_lock),
                write_boundary(),
            ),
        )
        .await
        .is_err()
    );
    assert_eq!(client.db().get_self_current_head().await, None);
    drop(guard);
    handle_signed_tool(
        &signed_invoke("rostra_post", serde_json::json!({"body":"new intent"})),
        &client,
        secret,
        write_lock,
        write_boundary(),
    )
    .await
    .expect("later independent write");
    assert_eq!(client.db().get_heads_self().await.len(), 1);
}
