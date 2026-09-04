#[cfg(unix)]
use std::fs::Permissions;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use tau_proto::{AgentId, Event, SessionAgentLoaded, SessionId};
use tempfile::TempDir;

use super::SessionRetentionReferences;
use crate::SessionStore;

fn append_load(store: &mut SessionStore, session: &SessionId, agent: &str) {
    store
        .append_session_event(
            session.as_str(),
            None,
            Event::SessionAgentLoaded(SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse(format!(
                    "init-{agent}"
                ))
                .expect("initialization id"),
                session_id: session.clone(),
                agent_id: AgentId::parse(agent).expect("agent id"),
                ephemeral: false,
            }),
        )
        .expect("append durable load");
}

/// Refresh streams an unchanged journal's appended suffix while preserving
/// references learned from its already validated prefix.
#[test]
fn refresh_extends_validated_journal_boundary_without_losing_prefix_references() {
    let temp = TempDir::new().expect("temp state");
    let sessions_dir = temp.path().join("sessions");
    let session = SessionId::parse("session").expect("session id");
    let mut store = SessionStore::open(&sessions_dir).expect("session store");
    append_load(&mut store, &session, "first");

    let mut references =
        SessionRetentionReferences::capture(&sessions_dir).expect("initial references");
    let first_boundary = references
        .journals
        .get(&session)
        .expect("session cursor")
        .offset;
    assert!(references.contains(&AgentId::parse("first").expect("first id")));

    append_load(&mut store, &session, "second");
    references.refresh().expect("suffix refresh");

    assert!(references.contains(&AgentId::parse("first").expect("first id")));
    assert!(references.contains(&AgentId::parse("second").expect("second id")));
    assert!(
        first_boundary
            < references
                .journals
                .get(&session)
                .expect("refreshed cursor")
                .offset
    );
}

/// Missing and malformed manifests remain noncanonical and therefore do not
/// manufacture ownership references.
#[test]
fn noncanonical_manifests_are_ignored() {
    let temp = TempDir::new().expect("temp state");
    let sessions_dir = temp.path().join("sessions");
    std::fs::create_dir_all(sessions_dir.join("missing")).expect("missing manifest session");
    std::fs::create_dir_all(sessions_dir.join("malformed")).expect("malformed manifest session");
    std::fs::write(sessions_dir.join("malformed/meta.json"), b"{bad").expect("malformed manifest");

    let references =
        SessionRetentionReferences::capture(&sessions_dir).expect("noncanonical sessions skipped");

    assert!(references.journals.is_empty());
}

/// A real manifest I/O failure is uncertainty, not proof that a valid-named
/// retained session has no agent references.
#[cfg(unix)]
#[test]
fn unreadable_manifest_aborts_reference_capture() {
    let temp = TempDir::new().expect("temp state");
    let sessions_dir = temp.path().join("sessions");
    let session = SessionId::parse("session").expect("session id");
    let mut store = SessionStore::open(&sessions_dir).expect("session store");
    append_load(&mut store, &session, "agent");
    drop(store);
    let manifest = sessions_dir.join("session/meta.json");
    let original = std::fs::metadata(&manifest)
        .expect("manifest metadata")
        .permissions();
    std::fs::set_permissions(&manifest, Permissions::from_mode(0o000))
        .expect("make manifest unreadable");

    let result = SessionRetentionReferences::capture(&sessions_dir);

    std::fs::set_permissions(&manifest, original).expect("restore manifest permissions");
    assert!(result.is_err(), "manifest I/O uncertainty must fail closed");
}

/// Replacing an already validated canonical journal aborts the refresh rather
/// than treating a new inode as a continuation of the old authority.
#[test]
fn replaced_journal_aborts_incremental_refresh() {
    let temp = TempDir::new().expect("temp state");
    let sessions_dir = temp.path().join("sessions");
    let session = SessionId::parse("session").expect("session id");
    let mut store = SessionStore::open(&sessions_dir).expect("session store");
    append_load(&mut store, &session, "first");
    drop(store);
    let mut references =
        SessionRetentionReferences::capture(&sessions_dir).expect("initial references");
    let journal = sessions_dir.join("session/events.cbor");
    let replacement = sessions_dir.join("session/events.replacement");
    std::fs::copy(&journal, &replacement).expect("copy journal bytes");
    std::fs::rename(&replacement, &journal).expect("replace journal inode");

    let result = references.refresh();

    assert!(result.is_err(), "replaced journal must fail closed");
}
