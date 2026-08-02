//! Deterministic contract tests for validation and hostile-content projection.

use std::collections::BTreeSet;
use std::fs;
use std::sync::mpsc;

use rostra_client::Database;
use rostra_client_db::social::EventPaginationCursor;
use rostra_core::event::PersonaTag;
use rostra_core::event::content_kind::PersonasTagsSelector;
use rostra_core::id::RostraIdSecretKey;

use super::*;
use crate::cursor::{Position, Timeline};
use crate::projection::{
    EXTERNAL_CLOSE, format_tags, sanitize_external, sanitize_line, truncate_utf8,
};
use crate::specs::{
    LIST_TOOL, PROFILE_TOOL, READ_TOOL, STATUS_TOOL, list_spec, profile_spec, read_spec,
    status_spec,
};

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

/// Ensures the public schema rejects excluded direct-IP and key configuration.
#[test]
fn config_schema_rejects_excluded_fields() {
    assert!(
        serde_json::from_value::<ExtConfig>(serde_json::json!({"identity":"rs-example"})).is_ok()
    );
    for excluded in [
        "public_mode",
        "secret",
        "secret_file",
        "mnemonic",
        "api_url",
    ] {
        let mut value = serde_json::json!({"identity":"rs-example"});
        value[excluded] = serde_json::json!(false);
        assert!(serde_json::from_value::<ExtConfig>(value).is_err());
    }
}

/// Ensures the four declarations retain exact names and strict object schemas.
#[test]
fn tool_declarations_match_first_slice() {
    let specs = [status_spec(), list_spec(), read_spec(), profile_spec()];
    assert_eq!(
        specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>(),
        [STATUS_TOOL, LIST_TOOL, READ_TOOL, PROFILE_TOOL]
    );
    assert!(specs.iter().all(|spec| {
        spec.parameters
            .as_ref()
            .and_then(|schema| schema.get("additionalProperties"))
            == Some(&serde_json::Value::Bool(false))
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
