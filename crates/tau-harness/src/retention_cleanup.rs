//! One ordered detached worker for startup retention policies.

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime};

use tau_core::{AgentPersistenceMode, SessionPersistenceMode};
use tau_proto::SessionId;

use crate::agent_cleanup::AgentCleanupSummary;
use crate::diagnostic_cleanup::DiagnosticCleanupSummary;
use crate::session_cleanup::SessionCleanupSummary;

/// Immutable startup retention inputs.
#[derive(Clone)]
pub(crate) struct RetentionCleanup {
    /// Tau state root containing the global agent store.
    pub(crate) state_dir: PathBuf,
    /// Canonical durable session root.
    pub(crate) sessions_dir: PathBuf,
    /// Session persistence policy for this harness.
    pub(crate) session_persistence: SessionPersistenceMode,
    /// Agent persistence policy for this harness.
    pub(crate) agent_persistence: AgentPersistenceMode,
    /// Session protected by this startup.
    pub(crate) current_session: SessionId,
    /// Optional whole-session retention.
    pub(crate) session_retention: Option<Duration>,
    /// Optional unreferenced-agent retention.
    pub(crate) agent_retention: Option<Duration>,
    /// Optional diagnostic-file retention.
    pub(crate) diagnostic_retention: Option<Duration>,
}

/// Starts one ordered opportunistic cleanup pass.
pub(crate) fn spawn_retention_cleanup(cleanup: RetentionCleanup) {
    if cleanup.session_persistence.is_ephemeral() && cleanup.agent_persistence.is_ephemeral() {
        return;
    }
    if let Err(error) = thread::Builder::new()
        .name("tau-retention-cleanup".to_owned())
        .spawn(move || run_retention_cleanup(cleanup, SystemTime::now()))
    {
        tracing::warn!(
            target: "tau_harness::retention_cleanup",
            %error,
            "failed to spawn retention cleanup thread"
        );
    }
}

fn run_retention_cleanup(cleanup: RetentionCleanup, now: SystemTime) {
    run_retention_cleanup_with_session_cleanup(
        cleanup,
        now,
        crate::session_cleanup::cleanup_old_sessions_at,
    );
}

fn run_retention_cleanup_with_session_cleanup(
    cleanup: RetentionCleanup,
    now: SystemTime,
    mut cleanup_sessions: impl FnMut(
        PathBuf,
        Duration,
        Vec<SessionId>,
        SystemTime,
    ) -> SessionCleanupSummary,
) {
    let mut failures = 0_u64;
    let mut session_authority_certain = true;
    if cleanup.session_persistence.is_durable() {
        let session_staging =
            crate::session_cleanup::finalize_detached_sessions(&cleanup.sessions_dir);
        failures += u64::from(session_staging.is_err());
        if let Err(error) = session_staging {
            session_authority_certain = false;
            tracing::warn!(target: "tau_harness::retention_cleanup", %error, "failed to finalize detached sessions");
        }
    }
    let agents_dir = cleanup.state_dir.join("agents");
    let mut agents = if cleanup.agent_persistence.is_durable() {
        crate::agent_cleanup::finalize_detached_agents(&agents_dir)
    } else {
        AgentCleanupSummary::default()
    };
    let mut sessions = SessionCleanupSummary::default();
    let mut diagnostics = DiagnosticCleanupSummary::default();
    if cleanup.session_persistence.is_durable()
        && let Some(retention) = cleanup.session_retention
    {
        sessions = cleanup_sessions(
            cleanup.sessions_dir.clone(),
            retention,
            vec![cleanup.current_session.clone()],
            now,
        );
        session_authority_certain &= sessions.failures == 0;
    }
    if cleanup.agent_persistence.is_durable()
        && session_authority_certain
        && let Some(retention) = cleanup.agent_retention
    {
        let current = crate::agent_cleanup::cleanup_agents(
            &agents_dir,
            &cleanup.sessions_dir,
            retention,
            now,
        );
        agents.scanned += current.scanned;
        agents.skipped_locked += current.skipped_locked;
        agents.skipped_referenced += current.skipped_referenced;
        agents.skipped_invalid += current.skipped_invalid;
        agents.detached += current.detached;
        agents.removed += current.removed;
        agents.failures += current.failures;
    }
    if cleanup.session_persistence.is_durable()
        && let Some(retention) = cleanup.diagnostic_retention
    {
        diagnostics = crate::diagnostic_cleanup::cleanup_diagnostics_at(
            &cleanup.sessions_dir,
            retention,
            now,
            &[cleanup.current_session],
        );
    }
    failures += agents.failures + sessions.failures + diagnostics.failures;
    tracing::info!(
        target: "tau_harness::retention_cleanup",
        sessions_scanned = sessions.scanned,
        sessions_detached = sessions.detached,
        sessions_removed = sessions.removed,
        agents_scanned = agents.scanned,
        agents_skipped_locked = agents.skipped_locked,
        agents_skipped_referenced = agents.skipped_referenced,
        agents_skipped_invalid = agents.skipped_invalid,
        agents_detached = agents.detached,
        agents_removed = agents.removed,
        diagnostics_scanned = diagnostics.scanned,
        diagnostics_removed = diagnostics.removed,
        failures,
        "startup retention cleanup finished"
    );
}
