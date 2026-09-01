//! Deterministic contract tests for validation and hostile-content projection.

mod lifecycle;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, mpsc};
use std::time::Instant;
use std::{fs, thread};

use rostra_client::{Client, Database, ExternalEventId};
use rostra_client_db::social::EventPaginationCursor;
use rostra_core::event::content_kind::{EventContentKind as _, Follow, PersonasTagsSelector};
use rostra_core::event::{Event as RostraEvent, EventKind, VerifiedEvent, VerifiedEventContent};
use rostra_core::id::RostraIdSecretKey;
use tau_proto::{
    Event, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter,
    ToolCancelRequest,
};
use tokio::sync::Notify;
use tracing::dispatcher::Dispatch;

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
    pause_before_test_publication_with_deadline_after_entry,
    pause_before_test_publication_without_deadline, short_event_id, validate_body,
};
use crate::tools::{ToolFailure, tool_error};

/// Phase-specific scheduling allowance beneath nextest's whole-test watchdog.
const TEST_GATE_WAIT: Duration = Duration::from_secs(5);
/// Serializes tests that install the process-global signed-publication gate.
static SIGNED_PUBLICATION_FIXTURE_LOCK: Mutex<()> = Mutex::new(());

/// Thread-safe protocol writer used to observe asynchronous extension output.
#[derive(Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl SharedWriter {
    /// Return every complete or in-progress frame written so far.
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().expect("shared writer lock").clone()
    }
}

/// Thread-safe in-memory stderr sink for one isolated Rostra logging dispatch.
#[derive(Clone, Default)]
struct CapturedStderr {
    /// Formatted log bytes written by the subscriber.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl CapturedStderr {
    /// Return the UTF-8 log stream captured from the isolated dispatch.
    fn text(&self) -> String {
        String::from_utf8(self.bytes.lock().expect("captured stderr lock").clone())
            .expect("tracing output is UTF-8")
    }
}

impl Write for CapturedStderr {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes
            .lock()
            .expect("captured stderr lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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

/// Ensures the production dispatcher obtains exact direct and two-hop counts
/// from a retained graph populated by signed hostile events.
#[tokio::test(flavor = "multi_thread")]
async fn status_dispatches_signed_retained_graph_counts() {
    const FANOUT: usize = 4;

    let temporary = tempfile::tempdir().expect("temporary directory");
    let self_secret = RostraIdSecretKey::generate();
    let self_id = self_secret.id();
    let hostile_secret = RostraIdSecretKey::generate();
    let database = Database::open(temporary.path().join("rostra.redb"), self_id)
        .await
        .expect("database");
    let client = Client::builder(self_id)
        .start_background_tasks(false)
        .db(database)
        .public_mode(false)
        .build()
        .await
        .expect("client");

    client
        .follow(
            self_secret,
            hostile_secret.id(),
            PersonasTagsSelector::default(),
        )
        .await
        .expect("direct follow");
    let mut parent = None;
    for _ in 0..FANOUT {
        let followee = RostraIdSecretKey::generate().id();
        let content = Follow {
            followee,
            persona: None,
            selector: None,
            persona_tags_selector: Some(PersonasTagsSelector::default()),
        }
        .serialize_cbor()
        .expect("follow content");
        let event = RostraEvent::builder_raw_content()
            .author(hostile_secret.id())
            .kind(EventKind::FOLLOW)
            .content(&content)
            .maybe_parent_prev(parent)
            .build();
        let signed = event.signed_by(hostile_secret);
        let verified = VerifiedEvent::verify_signed(hostile_secret.id(), signed)
            .expect("signed hostile follow");
        let verified = VerifiedEventContent::assume_verified(verified, content);
        parent = Some(verified.event_id().into());
        client.db().process_event_with_content(&verified).await;
    }

    let wot = client.self_wot_subscribe().snapshot();
    assert_eq!(wot.followees.len(), 1);
    assert_eq!(wot.extended.len(), FANOUT);

    let output = crate::tools::dispatch(
        &signed_invoke(STATUS_TOOL, serde_json::json!({})),
        &client,
        None,
        Arc::new(AsyncMutex::new(())),
        PostRateLimit::default(),
        Arc::new(Mutex::new(PostRateLimitWindow::default())),
        write_boundary(),
    )
    .await
    .expect("status");
    assert!(output.contains("known_direct_followees: 1\n"));
    assert!(output.contains(&format!("known_two_hop_identities: {FANOUT}\n")));
}

/// Ensures status owns no database traversal and obtains both graph counts
/// from exactly one coherent retained snapshot.
#[test]
fn status_uses_exactly_one_retained_snapshot_and_no_database() {
    let status_source = include_str!("tools/status.rs");
    assert_eq!(status_source.matches(".db()").count(), 0);
    assert_eq!(status_source.matches("get_followees").count(), 0);
    assert_eq!(status_source.matches("self_wot_subscribe").count(), 1);
    assert_eq!(status_source.matches(".snapshot()").count(), 1);
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

/// Ensures a failure at the configuration commit boundary preserves the active
/// client identity/path, notification worker and state, and full quota; a later
/// accepted replacement still resets runtime quota.
#[test]
fn configuration_commit_failure_preserves_active_runtime() {
    let runtime = RuntimeBuilder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let mut state = RostraState {
        client: None,
        identity_secret: None,
        state_dir: None,
        runtime: Some(runtime),
        running: Arc::new(Mutex::new(HashMap::new())),
        permits: Arc::new(Semaphore::new(MAX_CONCURRENT_TOOLS)),
        write_lock: Arc::new(AsyncMutex::new(())),
        post_rate_limit: PostRateLimit::default(),
        post_rate_limit_window: Arc::new(Mutex::new(PostRateLimitWindow::default())),
        notifications: Arc::new(Mutex::new(notification_state::State::default())),
        notifications_wake: Arc::new(Notify::new()),
        notifications_task: None,
        mandatory_output: MandatoryOutput::disconnected(),
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
    state
        .notifications
        .lock()
        .expect("notification state lock")
        .allocate_report_attempt()
        .expect("allocate notification attempt");
    let active_client = Arc::clone(state.client.as_ref().expect("active client"));
    let active_state_dir = state.state_dir.clone();
    let notification_task = state
        .runtime
        .as_ref()
        .expect("runtime")
        .spawn(std::future::pending::<()>())
        .abort_handle();
    state.notifications_task = Some(notification_task.clone());
    let mut reconfigure_event = tau_proto::Configure {
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
    *FAIL_CONFIGURATION_COMMIT
        .lock()
        .expect("configuration commit failure hook") = reconfigure_event.state_dir.clone();
    configure(
        &mut state,
        &reconfigure_event,
        ExtConfig {
            identity_mnemonic_secret: mnemonic_secret.to_owned(),
            post_rate_limit: limit,
        },
    )
    .expect_err("injected commit failure");
    assert!(Arc::ptr_eq(
        state.client.as_ref().expect("preserved client"),
        &active_client
    ));
    assert_eq!(state.identity_secret, Some(secret));
    assert_eq!(state.state_dir, active_state_dir);
    assert!(!notification_task.is_finished());
    assert_eq!(
        state
            .notifications
            .lock()
            .expect("notification state lock")
            .next_report_attempt(),
        1
    );
    assert!(
        state
            .post_rate_limit_window
            .lock()
            .expect("post rate-limit state lock")
            .reserve(limit)
            .is_err(),
        "failed candidate must preserve the full active quota"
    );
    reconfigure_event.state_dir = Some(temporary.path().join("successful-reconfigured-state"));
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

/// Ensures all approved tool schemas retain their required inputs, strict
/// object boundary, and authority- or resource-bearing argument constraints.
#[test]
fn tool_declarations_match_approved_slice() {
    let specs = [
        (
            status_spec(),
            None,
            &[][..],
            serde_json::json!({"properties": {}}),
        ),
        (
            list_spec(),
            Some(&["timeline"][..]),
            &["timeline", "author", "cursor", "limit"][..],
            serde_json::json!({
                "properties": {
                    "timeline": {"type": "string", "enum": ["following", "network", "author"]},
                    "author": {"type": "string"},
                    "cursor": {"type": "string"},
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_PAGE_SIZE,
                        "default": DEFAULT_PAGE_SIZE,
                    },
                },
            }),
        ),
        (
            read_spec(),
            Some(&["post_id"][..]),
            &["post_id"][..],
            serde_json::json!({"properties": {"post_id": {"type": "string"}}}),
        ),
        (
            profile_spec(),
            Some(&["identity"][..]),
            &["identity"][..],
            serde_json::json!({"properties": {"identity": {"type": "string"}}}),
        ),
        (
            crate::specs::notifications_spec(),
            Some(&["enabled"][..]),
            &["enabled"][..],
            serde_json::json!({"properties": {"enabled": {"type": "boolean"}}}),
        ),
        (
            post_spec(),
            Some(&["body"][..]),
            &["body", "reply_to", "persona_tags"][..],
            serde_json::json!({
                "properties": {
                    "body": {"type": "string", "minLength": 1, "maxLength": MAX_DJOT_BYTES},
                    "reply_to": {"type": "string"},
                    "persona_tags": {
                        "type": "array",
                        "items": {"type": "string", "minLength": 1, "maxLength": 32},
                        "maxItems": 16,
                        "default": [],
                    },
                },
            }),
        ),
        (
            react_spec(),
            Some(&["post_id", "reaction"][..]),
            &["post_id", "reaction"][..],
            serde_json::json!({
                "properties": {
                    "post_id": {"type": "string"},
                    "reaction": {"type": "string", "minLength": 1, "maxLength": 8},
                },
            }),
        ),
        (
            follow_spec(),
            Some(&["identity"][..]),
            &["identity"][..],
            serde_json::json!({"properties": {"identity": {"type": "string"}}}),
        ),
        (
            unfollow_spec(),
            Some(&["identity"][..]),
            &["identity"][..],
            serde_json::json!({"properties": {"identity": {"type": "string"}}}),
        ),
        (
            profile_update_spec(),
            Some(&["display_name", "bio"][..]),
            &["display_name", "bio"][..],
            serde_json::json!({
                "properties": {
                    "display_name": {"type": "string", "maxLength": 100},
                    "bio": {"type": "string", "maxLength": 1000},
                },
            }),
        ),
        (
            vote_spec(),
            Some(&["post_id", "vote"][..]),
            &["post_id", "vote"][..],
            serde_json::json!({
                "properties": {
                    "post_id": {"type": "string"},
                    "vote": {"type": "string", "enum": ["up", "down", "clear"]},
                },
            }),
        ),
    ];

    assert_eq!(
        specs
            .iter()
            .map(|(spec, ..)| spec.name.as_str())
            .collect::<Vec<_>>(),
        [
            STATUS_TOOL,
            LIST_TOOL,
            READ_TOOL,
            PROFILE_TOOL,
            crate::specs::NOTIFICATIONS_TOOL,
            POST_TOOL,
            REACT_TOOL,
            FOLLOW_TOOL,
            UNFOLLOW_TOOL,
            PROFILE_UPDATE_TOOL,
            VOTE_TOOL,
        ]
    );
    for (spec, required, property_names, fragment) in specs {
        let schema = spec.parameters.as_ref().expect("function schema");
        assert_eq!(schema["type"], "object", "{}", spec.name);
        assert_eq!(schema["additionalProperties"], false, "{}", spec.name);
        match required {
            Some(required) => {
                assert_eq!(
                    schema["required"],
                    serde_json::json!(required),
                    "{}",
                    spec.name
                );
            }
            None => assert!(
                schema.get("required").is_none(),
                "{} must not require arguments",
                spec.name
            ),
        }
        assert_eq!(
            schema["properties"]
                .as_object()
                .expect("object properties")
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            property_names.iter().copied().collect(),
            "{}",
            spec.name
        );
        assert_schema_fragment(schema, &fragment, spec.name.as_str());
    }
}

/// Recursively checks object fragments while comparing leaves exactly, so
/// tool-description prose remains outside this contract oracle.
fn assert_schema_fragment(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    tool_name: &str,
) {
    match expected {
        serde_json::Value::Object(expected) => {
            let actual = actual.as_object().expect("schema object");
            for (key, expected) in expected {
                assert_schema_fragment(
                    actual
                        .get(key)
                        .unwrap_or_else(|| panic!("{tool_name} is missing {key}")),
                    expected,
                    tool_name,
                );
            }
        }
        _ => assert_eq!(actual, expected, "{tool_name} schema fragment"),
    }
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
    drop(writer);
    runner
        .join()
        .expect("Rostra runner thread")
        .expect("Rostra runner succeeds");

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

/// Ensures all timeline headers use their approved lowercase schema spelling.
#[test]
fn timeline_headers_are_lowercase() {
    assert_eq!(Timeline::Following.as_str(), "following");
    assert_eq!(Timeline::Network.as_str(), "network");
    assert_eq!(Timeline::Author.as_str(), "author");
}

/// Ensures dropping RostraState remains bounded with a retained call and
/// notification task.
#[test]
fn runtime_shutdown_is_bounded() {
    let runtime = RuntimeBuilder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let running_task = runtime.spawn(std::future::pending::<()>());
    let notification_task = runtime.spawn(std::future::pending::<()>());
    let running = Arc::new(Mutex::new(HashMap::from([(
        tau_proto::ToolCallId::from("pending-call"),
        RunningCall {
            abort: running_task.abort_handle(),
            tool_name: tau_proto::ToolName::new(STATUS_TOOL),
            publishing: false,
        },
    )])));
    let state = RostraState {
        client: None,
        identity_secret: None,
        state_dir: None,
        runtime: Some(runtime),
        running,
        permits: Arc::new(Semaphore::new(MAX_CONCURRENT_TOOLS)),
        write_lock: Arc::new(AsyncMutex::new(())),
        post_rate_limit: PostRateLimit::default(),
        post_rate_limit_window: Arc::new(Mutex::new(PostRateLimitWindow::default())),
        notifications: Arc::new(Mutex::new(notification_state::State::default())),
        notifications_wake: Arc::new(Notify::new()),
        notifications_task: Some(notification_task.abort_handle()),
        mandatory_output: MandatoryOutput::disconnected(),
    };
    let (done_tx, done_rx) = mpsc::channel();
    let shutdown = thread::spawn(move || {
        drop(state);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("RostraState drop must bound shutdown");
    shutdown.join().expect("shutdown helper thread");
}

/// Construct a minimal authenticated tool invocation.
fn signed_invoke(name: &str, arguments: serde_json::Value) -> tau_proto::ToolStarted {
    tau_proto::ToolStarted {
        invocation_policy: tau_proto::ToolInvocationPolicy::default(),
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
/// completion, and never emits a late result or retries it. The test's
/// one-second semantic deadline begins at the controlled post gate. Bounded
/// phase watchdogs retain direct-test diagnostics while allowing fresh
/// activation scheduling margin beneath nextest's whole-test liveness bound.
#[test]
fn signed_write_timeout_and_cancellation_retain_the_committing_lane() {
    let _fixture = SIGNED_PUBLICATION_FIXTURE_LOCK
        .lock()
        .expect("signed publication fixture lock");
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let state_dir = temporary.path().join("state");
    let (extension_input, harness_input) = UnixStream::pair().expect("input stream pair");
    let output = SharedWriter::default();
    let runner_output = output.clone();
    let stderr = CapturedStderr::default();
    let trace_writer = stderr.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("tau_ext_rostra=debug,warn")
        .with_writer(move || trace_writer.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    let dispatch = Dispatch::new(subscriber);
    let runner = thread::spawn(move || {
        tracing::dispatcher::with_default(&dispatch, || {
            run(extension_input, runner_output).map_err(|error| error.to_string())
        })
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
    let timeout_gate =
        pause_before_test_publication_with_deadline_after_entry(timeout_invoke.call_id.clone());
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
            timeout_invoke.clone(),
        )))
        .expect("start timeout post");
    writer.flush().expect("flush timeout post");
    timeout_gate
        .entered
        .recv_timeout(TEST_GATE_WAIT)
        .expect("timeout post reaches publication-admission boundary");
    assert!(
        timeout_gate.publication_admitted(),
        "timeout post enters its test gate only after publication admission"
    );
    wait_for_output_event(&output, |event| {
        matches!(
            event,
            Event::ToolErrorReported(error) if error.call_id == timeout_invoke.call_id
        )
    });
    let Some(Event::ToolErrorReported(timeout)) =
        output_events(&output).into_iter().find(|event| {
            matches!(
                event,
                Event::ToolErrorReported(error) if error.call_id == timeout_invoke.call_id
            )
        })
    else {
        panic!("timeout post reports one early error");
    };
    assert!(
        timeout.message.starts_with("timeout:"),
        "admitted post deadline reports the timeout category"
    );
    timeout_gate
        .release
        .send(())
        .expect("release timeout publication");
    timeout_gate
        .committed
        .recv_timeout(TEST_GATE_WAIT)
        .expect("timeout publication commits exactly once");

    let mut cancelled_invoke = signed_invoke(
        "rostra_post",
        serde_json::json!({"body":"cancel after publication admission"}),
    );
    cancelled_invoke.call_id = tau_proto::ToolCallId::new("cancel-after-admission");
    let cancelled_gate = pause_before_test_publication(cancelled_invoke.call_id.clone());
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
            cancelled_invoke.clone(),
        )))
        .expect("start cancelled post");
    writer.flush().expect("flush cancelled post");
    cancelled_gate
        .entered
        .recv_timeout(TEST_GATE_WAIT)
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
    cancelled_gate
        .release
        .send(())
        .expect("release cancelled publication");
    cancelled_gate
        .committed
        .recv_timeout(TEST_GATE_WAIT)
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
    let stderr = stderr.text();
    assert!(!stderr.contains("local_commit"));
    assert!(!stderr.contains("timeout-after-admission"));
    assert!(!stderr.contains("cancel-after-admission"));

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

/// Keeps the diagnostic event-ID projection at its fixed twelve-character
/// prefix.
#[test]
fn short_event_id_keeps_the_fixed_diagnostic_prefix() {
    let full_event_id = rostra_core::EventId::from_bytes([0x42; 32]);
    let expected_event_id = short_event_id(full_event_id);
    let full_event_id = full_event_id.to_string();

    assert_eq!(expected_event_id.chars().count(), 12);
    assert_eq!(
        expected_event_id,
        full_event_id.chars().take(12).collect::<String>()
    );
    assert_ne!(expected_event_id, full_event_id);
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
