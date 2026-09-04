//! Best-effort startup cleanup for expired unreferenced durable agents.

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use fs2::FileExt as _;
use tau_proto::AgentId;

static CLEANUP_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Aggregate content-free counters from one agent cleanup pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AgentCleanupSummary {
    /// Real valid-ID agent directories inspected.
    pub(crate) scanned: u64,
    /// Candidates retained because a lock was held.
    pub(crate) skipped_locked: u64,
    /// Candidates retained because a surviving session references them.
    pub(crate) skipped_referenced: u64,
    /// Candidates retained because exact eligibility was unavailable.
    pub(crate) skipped_invalid: u64,
    /// Agent directories atomically detached.
    pub(crate) detached: u64,
    /// Detached directories recursively removed.
    pub(crate) removed: u64,
    /// Candidate or staging operations that failed.
    pub(crate) failures: u64,
}

/// Shared immutable authority for candidate-specific cleanup decisions.
struct AgentCleanupContext<'a> {
    /// Global durable agent root.
    agents_dir: &'a Path,
    /// Canonical durable session root.
    sessions_dir: &'a Path,
    /// Required minimum age for both clocks.
    retention: Duration,
    /// One wall-clock snapshot shared by the pass.
    now: SystemTime,
}

struct AgentCleanupHooks<'a> {
    /// Deterministic cut immediately before the candidate-specific reference
    /// scan.
    before_rescan: &'a mut dyn FnMut(&AgentId),
    /// Tombstone publication operation.
    create_tombstone: &'a mut dyn FnMut(&Path, &AgentId) -> io::Result<()>,
    /// Atomic detach rename operation.
    rename: &'a mut dyn FnMut(&Path, &Path) -> io::Result<()>,
    /// Detached-tree recursive removal operation.
    remove_dir: &'a mut dyn FnMut(&Path) -> io::Result<()>,
}

/// Finalizes prior committed detaches regardless of the current policy.
pub(crate) fn finalize_detached_agents(agents_dir: &Path) -> AgentCleanupSummary {
    finalize_detached_agents_with(agents_dir, |path| fs::remove_dir_all(path))
}

fn finalize_detached_agents_with(
    agents_dir: &Path,
    mut remove_dir: impl FnMut(&Path) -> io::Result<()>,
) -> AgentCleanupSummary {
    let mut summary = AgentCleanupSummary::default();
    let cleanup_dir = detached_agents_dir(agents_dir);
    let entries = match fs::read_dir(&cleanup_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return summary,
        Err(error) => {
            summary.failures += 1;
            tracing::warn!(
                target: "tau_harness::retention_cleanup",
                path = %cleanup_dir.display(),
                %error,
                "failed to list detached agent staging"
            );
            return summary;
        }
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                summary.failures += 1;
                tracing::warn!(target: "tau_harness::retention_cleanup", %error, "failed to inspect detached agent staging entry");
                continue;
            }
        };
        match remove_dir(&path) {
            Ok(()) => summary.removed += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                summary.failures += 1;
                tracing::warn!(
                    target: "tau_harness::retention_cleanup",
                    path = %path.display(),
                    %error,
                    "failed to remove detached agent directory"
                );
            }
        }
    }
    summary
}

/// Removes expired agents only when exact age and surviving-session authority
/// agree.
pub(crate) fn cleanup_agents(
    agents_dir: &Path,
    sessions_dir: &Path,
    retention: Duration,
    now: SystemTime,
) -> AgentCleanupSummary {
    let mut before_rescan = |_: &AgentId| {};
    let mut create_tombstone =
        |agents_dir: &Path, agent_id: &AgentId| create_tombstone(agents_dir, agent_id);
    let mut rename = |source: &Path, destination: &Path| fs::rename(source, destination);
    let mut remove_dir = |path: &Path| fs::remove_dir_all(path);
    cleanup_agents_with_hooks(
        agents_dir,
        sessions_dir,
        retention,
        now,
        AgentCleanupHooks {
            before_rescan: &mut before_rescan,
            create_tombstone: &mut create_tombstone,
            rename: &mut rename,
            remove_dir: &mut remove_dir,
        },
    )
}

#[cfg(test)]
fn cleanup_agents_with_before_rescan(
    agents_dir: &Path,
    sessions_dir: &Path,
    retention: Duration,
    now: SystemTime,
    before_rescan: impl FnMut(&AgentId),
) -> AgentCleanupSummary {
    let mut before_rescan = before_rescan;
    let mut create_tombstone =
        |agents_dir: &Path, agent_id: &AgentId| create_tombstone(agents_dir, agent_id);
    let mut rename = |source: &Path, destination: &Path| fs::rename(source, destination);
    let mut remove_dir = |path: &Path| fs::remove_dir_all(path);
    cleanup_agents_with_hooks(
        agents_dir,
        sessions_dir,
        retention,
        now,
        AgentCleanupHooks {
            before_rescan: &mut before_rescan,
            create_tombstone: &mut create_tombstone,
            rename: &mut rename,
            remove_dir: &mut remove_dir,
        },
    )
}

fn cleanup_agents_with_hooks(
    agents_dir: &Path,
    sessions_dir: &Path,
    retention: Duration,
    now: SystemTime,
    mut hooks: AgentCleanupHooks<'_>,
) -> AgentCleanupSummary {
    let mut summary = AgentCleanupSummary::default();
    let coarse_references = match surviving_references(sessions_dir) {
        Ok(references) => references,
        Err(error) => {
            summary.failures += 1;
            tracing::warn!(
                target: "tau_harness::retention_cleanup",
                %error,
                "canonical session uncertainty aborted agent cleanup"
            );
            return summary;
        }
    };
    let context = AgentCleanupContext {
        agents_dir,
        sessions_dir,
        retention,
        now,
    };
    let entries = match fs::read_dir(agents_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return summary,
        Err(error) => {
            summary.failures += 1;
            tracing::warn!(target: "tau_harness::retention_cleanup", %error, "failed to list agents for cleanup");
            return summary;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                summary.failures += 1;
                tracing::warn!(target: "tau_harness::retention_cleanup", %error, "failed to inspect agent entry");
                continue;
            }
        };
        let Ok(file_type) = entry.file_type() else {
            summary.failures += 1;
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        let Ok(agent_id) = AgentId::parse(name) else {
            continue;
        };
        let Ok(directory_metadata) = entry.metadata() else {
            summary.failures += 1;
            continue;
        };
        summary.scanned += 1;
        if coarse_references.contains(&agent_id) {
            summary.skipped_referenced += 1;
            continue;
        }
        consider_agent(
            &context,
            &entry.path(),
            &directory_metadata,
            &agent_id,
            &mut summary,
            &mut hooks,
        );
    }
    summary
}

fn consider_agent(
    context: &AgentCleanupContext<'_>,
    agent_dir: &Path,
    directory_metadata: &fs::Metadata,
    agent_id: &AgentId,
    summary: &mut AgentCleanupSummary,
    hooks: &mut AgentCleanupHooks<'_>,
) {
    let _lock = match try_lock_existing(&agent_dir.join("lock")) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            summary.skipped_locked += 1;
            return;
        }
        Err(error) => {
            summary.skipped_invalid += 1;
            tracing::warn!(target: "tau_harness::retention_cleanup", %agent_id, %error, "agent is ineligible for cleanup");
            return;
        }
    };
    if !path_still_names_directory(agent_dir, directory_metadata) {
        summary.skipped_invalid += 1;
        return;
    }
    let evidence = match tau_core::inspect_agent_retention_evidence(agent_dir, agent_id) {
        Ok(evidence) => evidence,
        Err(error) => {
            summary.skipped_invalid += 1;
            tracing::warn!(target: "tau_harness::retention_cleanup", %agent_id, %error, "agent checkpoint is not exact cleanup authority");
            return;
        }
    };
    if let Err(error) =
        tau_core::AgentJournalSnapshot::capture(context.agents_dir, [agent_id.clone()])
    {
        summary.skipped_invalid += 1;
        tracing::warn!(target: "tau_harness::retention_cleanup", %agent_id, %error, "agent journal failed strict retention replay");
        return;
    }
    if !path_still_names_directory(agent_dir, directory_metadata) {
        summary.skipped_invalid += 1;
        return;
    }
    if !expired(context.now, evidence.last_touched, context.retention)
        || !expired(context.now, evidence.journal_modified, context.retention)
    {
        return;
    }
    (hooks.before_rescan)(agent_id);
    match surviving_references(context.sessions_dir) {
        Ok(references) if references.contains(agent_id) => {
            summary.skipped_referenced += 1;
            return;
        }
        Ok(_) => {}
        Err(error) => {
            summary.failures += 1;
            tracing::warn!(target: "tau_harness::retention_cleanup", %agent_id, %error, "session rescan aborted agent deletion");
            return;
        }
    }
    if !path_still_names_directory(agent_dir, directory_metadata) {
        summary.skipped_invalid += 1;
        return;
    }
    if let Err(error) = (hooks.create_tombstone)(context.agents_dir, agent_id) {
        summary.failures += 1;
        tracing::warn!(target: "tau_harness::retention_cleanup", %agent_id, %error, "failed to commit retired agent id");
        return;
    }
    if !path_still_names_directory(agent_dir, directory_metadata) {
        summary.skipped_invalid += 1;
        return;
    }
    let detached = match detach_agent_dir_with(
        context.agents_dir,
        agent_dir,
        agent_id,
        hooks.rename,
    ) {
        Ok(path) => path,
        Err(error) => {
            summary.failures += 1;
            tracing::warn!(target: "tau_harness::retention_cleanup", %agent_id, %error, "failed to detach expired agent");
            return;
        }
    };
    summary.detached += 1;
    match (hooks.remove_dir)(&detached) {
        Ok(()) => summary.removed += 1,
        Err(error) => {
            summary.failures += 1;
            tracing::warn!(target: "tau_harness::retention_cleanup", path = %detached.display(), %error, "failed to remove detached agent");
        }
    }
}

fn surviving_references(sessions_dir: &Path) -> io::Result<HashSet<AgentId>> {
    let mut references = HashSet::new();
    for (session_id, _) in tau_core::list_session_metas(sessions_dir)? {
        let session_dir = sessions_dir.join(session_id.as_str());
        references.extend(
            tau_core::read_session_ever_loaded_agents(&session_dir, &session_id)
                .map_err(io::Error::other)?,
        );
    }
    Ok(references)
}

fn expired(now: SystemTime, timestamp: SystemTime, retention: Duration) -> bool {
    now.duration_since(timestamp)
        .is_ok_and(|age| retention <= age)
}

fn try_lock_existing(path: &Path) -> io::Result<Option<File>> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "lock is not a file",
        ));
    }
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

fn path_still_names_directory(path: &Path, opened: &fs::Metadata) -> bool {
    let Ok(current) = fs::symlink_metadata(path) else {
        return false;
    };
    if current.file_type().is_symlink() || !current.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        opened.dev() == current.dev() && opened.ino() == current.ino()
    }
    #[cfg(not(unix))]
    {
        opened.modified().ok() == current.modified().ok()
    }
}

fn create_tombstone(agents_dir: &Path, agent_id: &AgentId) -> io::Result<()> {
    create_tombstone_with(
        agents_dir,
        agent_id,
        create_new_tombstone,
        File::sync_all,
        |_, directory| directory.sync_all(),
    )
}

fn create_tombstone_with(
    agents_dir: &Path,
    agent_id: &AgentId,
    mut create_new: impl FnMut(&Path) -> io::Result<File>,
    mut sync_file: impl FnMut(&File) -> io::Result<()>,
    mut sync_directory: impl FnMut(&Path, &File) -> io::Result<()>,
) -> io::Result<()> {
    let retired_dir = tau_core::retired_agents_dir(agents_dir);
    match fs::symlink_metadata(&retired_dir) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "retired agent root is not a real directory",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&retired_dir)?,
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    fs::set_permissions(&retired_dir, fs::Permissions::from_mode(0o700))?;
    let path = tau_core::retired_agent_tombstone(agents_dir, agent_id);
    let file = match create_new(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            open_existing_tombstone(&path)?
        }
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "retired agent tombstone is not empty",
        ));
    }
    sync_file(&file)?;
    sync_directory(&retired_dir, &open_directory_nofollow(&retired_dir)?)?;
    if let Some(parent) = retired_dir.parent() {
        sync_directory(parent, &open_directory_nofollow(parent)?)?;
    }
    Ok(())
}

fn create_new_tombstone(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

fn open_existing_tombstone(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "retired agent tombstone is not a regular file",
        ));
    }
    Ok(file)
}

fn open_directory_nofollow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options.open(path)
}

fn detach_agent_dir_with(
    agents_dir: &Path,
    path: &Path,
    agent_id: &AgentId,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let cleanup_dir = detached_agents_dir(agents_dir);
    fs::create_dir_all(&cleanup_dir)?;
    loop {
        let suffix = CLEANUP_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let detached = cleanup_dir.join(format!(
            "{}-{}-{suffix}",
            agent_id.as_str(),
            std::process::id()
        ));
        match rename(path, &detached) {
            Ok(()) => return Ok(detached),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

fn detached_agents_dir(agents_dir: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(
        agents_dir
            .file_name()
            .unwrap_or_else(|| OsStr::new("agents")),
    );
    name.push(".cleanup");
    agents_dir.with_file_name(name)
}
