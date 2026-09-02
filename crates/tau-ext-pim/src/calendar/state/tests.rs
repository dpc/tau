use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::Path;

use super::*;

#[cfg(unix)]
fn file_mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn recent_calendar_log_ignores_invalid_entries_and_keeps_limit() {
    // Logs can be manually truncated or edited during debugging. Invalid
    // lines should not break `:calendar log last`, and the tail limit should
    // still be enforced.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let state = StateStore::open(temp.path().join("state")).expect("state");
    state
        .append_calendar_log(&CalendarLogEntry::tool("list_events", "ok"))
        .expect("append first");
    fs::write(
        temp.path().join("state/logs/calendar.jsonl"),
        b"not-json\n{\"schema\":1}\n{\"schema\":0,\"ts_unix_ms\":2,\"kind\":\"tool\",\"command\":\"read_event\",\"status\":\"ok\"}\n{\"schema\":0,\"ts_unix_ms\":3,\"kind\":\"tool\",\"command\":\"free_busy\",\"status\":\"ok\"}\n",
    )
    .expect("rewrite log");

    let entries = state.recent_calendar_log(1).expect("recent log");

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "free_busy");
}

#[test]
fn pending_calendar_changes_are_deduplicated_and_private() {
    // Calendar mutations can notify attendees, so they are persisted for
    // explicit user review and identical repeated tool calls reuse one id.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let state = StateStore::open(temp.path().join("state")).expect("state");
    let mut change = CalendarChangeApproval::pending("create_event", "google", "primary");
    change.title = Some("Team sync".to_owned());
    change.start = Some("2026-05-28T12:00:00Z".to_owned());
    change.end = Some("2026-05-28T13:00:00Z".to_owned());

    let first = state.pending_change(&change).expect("pending change");
    let second = state.pending_change(&change).expect("same pending change");

    assert_eq!(first, second);
    let pending = state.list_pending_changes().expect("pending list");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].title.as_deref(), Some("Team sync"));
    #[cfg(unix)]
    assert_eq!(
        file_mode(
            &temp
                .path()
                .join("state/approvals/calendar-change/pending/1.json")
        ),
        0o600
    );
}

/// Semantic runtime identities must not change the exact raw JSON approval
/// representation consumed after process restart.
#[test]
fn pending_calendar_change_preserves_raw_identity_bytes() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let state = StateStore::open(temp.path().join("state")).expect("state");
    let mut change = CalendarChangeApproval::pending("update_event", "google", "primary");
    change.event_id = Some("evt/raw".to_owned());
    change.etag = Some("etag/raw".to_owned());
    change.title = Some("Team sync".to_owned());

    let id = state.pending_change(&change).expect("pending change");
    let bytes = fs::read(
        temp.path()
            .join(format!("state/approvals/calendar-change/pending/{id}.json")),
    )
    .expect("approval bytes");

    assert_eq!(
        bytes,
        br#"{
  "schema": 0,
  "id": "1",
  "kind": "calendar_change",
  "status": "pending",
  "command": "update_event",
  "account": "google",
  "calendar": "primary",
  "event_id": "evt/raw",
  "etag": "etag/raw",
  "title": "Team sync",
  "reason": "user_approval_required"
}"#
    );
}

/// Calendar write-time typing must not change exact schema-0 approval bytes or
/// optional-field omission at the persistence adapter.
#[test]
fn pending_calendar_change_preserves_raw_event_time_bytes() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let state = StateStore::open(temp.path().join("state")).expect("state");
    let mut change = CalendarChangeApproval::pending("create_event", "google", "primary");
    change.title = Some("Offset meeting".to_owned());
    change.start = Some("2026-05-28T14:00:00+02:00".to_owned());
    change.end = Some("2026-05-28T15:00:00+02:00".to_owned());

    let id = state.pending_change(&change).expect("pending change");
    let bytes = fs::read(
        temp.path()
            .join(format!("state/approvals/calendar-change/pending/{id}.json")),
    )
    .expect("approval bytes");

    assert_eq!(
        bytes,
        br#"{
  "schema": 0,
  "id": "1",
  "kind": "calendar_change",
  "status": "pending",
  "command": "create_event",
  "account": "google",
  "calendar": "primary",
  "title": "Offset meeting",
  "start": "2026-05-28T14:00:00+02:00",
  "end": "2026-05-28T15:00:00+02:00",
  "reason": "user_approval_required"
}"#
    );
}

#[test]
fn claimed_calendar_change_can_be_released_before_dispatch() {
    // Local preparation can fail after the approval record is claimed. When
    // the caller proves mutation dispatch did not begin, the claim must become
    // pending again so the user can retry or deny it.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let state = StateStore::open(temp.path().join("state")).expect("state");
    let mut change = CalendarChangeApproval::pending("update_event", "google", "primary");
    change.event_id = Some("evt".to_owned());
    change.etag = Some("3560073119029470".to_owned());
    change.start = Some("2026-05-29T15:00:00Z".to_owned());

    let id = state.pending_change(&change).expect("pending change");
    let claimed = state.claim_change(&id).expect("claim");
    assert_eq!(claimed.status, "pending");
    assert!(state.change_sending_exists(&id).expect("sending exists"));

    state
        .release_claimed_change(&id)
        .expect("release claimed change");

    assert!(state.change_pending_exists(&id).expect("pending exists"));
    assert!(!state.change_sending_exists(&id).expect("sending gone"));
    assert_eq!(
        state
            .pending_change_by_id(&id)
            .expect("pending loaded")
            .status,
        "pending"
    );
}

#[test]
fn calendar_change_approval_validation_rejects_unsafe_persisted_records() {
    // Persisted approvals gate side-effecting calendar writes. Keep representative
    // rejection cases pinned so validation helper refactors cannot accidentally
    // skip identity, status, metadata, field, description, or attendee checks.
    #[derive(Clone, Copy)]
    enum Mutation {
        UnsupportedSchema,
        EmbeddedIdMismatch,
        StatusMismatch,
        UnsupportedCommand,
        UnsafeTitle,
        UnsafeDescription,
        TooManyAttendees,
        UnsafeAttendee,
    }

    let base = || {
        let mut approval = CalendarChangeApproval::pending("create_event", "google", "primary");
        approval.id = "1".to_owned();
        approval
    };

    use Mutation::{
        EmbeddedIdMismatch, StatusMismatch, TooManyAttendees, UnsafeAttendee, UnsafeDescription,
        UnsafeTitle, UnsupportedCommand, UnsupportedSchema,
    };

    for (case, mutate, expected_id, expected_error) in [
        (
            "unsupported schema",
            UnsupportedSchema,
            Some("1"),
            "unsupported schema",
        ),
        (
            "embedded id mismatch",
            EmbeddedIdMismatch,
            Some("2"),
            "mismatched embedded id",
        ),
        (
            "status mismatch",
            StatusMismatch,
            Some("1"),
            "mismatched embedded status",
        ),
        (
            "unsupported command",
            UnsupportedCommand,
            Some("1"),
            "unsafe metadata",
        ),
        (
            "unsafe title",
            UnsafeTitle,
            Some("1"),
            "field `title` contains unsafe text",
        ),
        (
            "unsafe description",
            UnsafeDescription,
            Some("1"),
            "field `description` contains unsafe text",
        ),
        (
            "too many attendees",
            TooManyAttendees,
            Some("1"),
            "too many attendees",
        ),
        (
            "unsafe attendee",
            UnsafeAttendee,
            Some("1"),
            "unsafe attendee",
        ),
    ] {
        let mut approval = base();
        match mutate {
            UnsupportedSchema => approval.schema = CHANGE_SCHEMA + 1,
            EmbeddedIdMismatch => {}
            StatusMismatch => approval.status = "approved".to_owned(),
            UnsupportedCommand => approval.command = "unknown".to_owned(),
            UnsafeTitle => approval.title = Some("unsafe\nline".to_owned()),
            UnsafeDescription => approval.description = Some("unsafe\u{202e}text".to_owned()),
            TooManyAttendees => {
                approval.attendees = Some(vec!["person@example.test".to_owned(); 201])
            }
            UnsafeAttendee => approval.attendees = Some(vec!["unsafe\nattendee".to_owned()]),
        }

        let error =
            validate_calendar_change_approval(&approval, "pending", expected_id).expect_err(case);

        assert!(
            error.contains(expected_error),
            "{case}: `{error}` did not contain `{expected_error}`"
        );
    }
}

#[test]
fn google_auth_tokens_and_pending_requests_are_private() {
    // Google refresh tokens and device codes are secrets. Persist them only
    // under owner-only files named by a hash of the account id.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let state = StateStore::open(temp.path().join("state")).expect("state");

    state
        .save_google_refresh_token("work/account", "refresh-token")
        .expect("save refresh token");
    assert_eq!(
        state
            .google_refresh_token("work/account")
            .expect("load refresh token")
            .as_deref(),
        Some("refresh-token")
    );
    let auth_path = state.google_auth_path("work/account");
    let auth_file = file_name(&auth_path).expect("auth filename");
    assert!(!auth_file.contains("work"));
    assert!(!auth_file.contains('/'));
    #[cfg(unix)]
    assert_eq!(
        file_mode(&temp.path().join("state").join(&auth_path)),
        0o600
    );

    let pending = GooglePendingAuth::new(
        "work/account",
        "device-code",
        "USER-CODE",
        "https://example.test/device",
        600,
        5,
    );
    state
        .save_pending_google_auth(&pending)
        .expect("save pending auth");
    assert_eq!(
        state
            .pending_google_auth("work/account")
            .expect("load pending auth")
            .device_code,
        "device-code"
    );
    let pending_path = state.google_pending_auth_path("work/account");
    #[cfg(unix)]
    assert_eq!(
        file_mode(&temp.path().join("state").join(&pending_path)),
        0o600
    );
    state
        .clear_pending_google_auth("work/account")
        .expect("clear pending auth");
    assert!(state.pending_google_auth("work/account").is_err());
}

#[cfg(unix)]
#[test]
fn calendar_log_files_are_owner_only() {
    // Calendar logs contain schedule metadata. Existing permissive paths
    // must be tightened on both append and read paths.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let state_dir = temp.path().join("state");
    fs::create_dir_all(state_dir.join("logs")).expect("mkdir");
    fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o755)).expect("chmod state");
    let log_path = state_dir.join("logs/calendar.jsonl");
    fs::write(&log_path, b"").expect("log");
    fs::set_permissions(&log_path, fs::Permissions::from_mode(0o644)).expect("chmod log");

    let state = StateStore::open(state_dir.clone()).expect("state");
    state
        .append_calendar_log(&CalendarLogEntry::tool("list_events", "ok"))
        .expect("append log");
    assert_eq!(file_mode(&state_dir), 0o700);
    assert_eq!(file_mode(&state_dir.join("logs")), 0o700);
    assert_eq!(file_mode(&log_path), 0o600);

    fs::set_permissions(&log_path, fs::Permissions::from_mode(0o644)).expect("chmod log");
    let entries = state.recent_calendar_log(1).expect("recent log");
    assert_eq!(entries.len(), 1);
    assert_eq!(file_mode(&log_path), 0o600);
}
