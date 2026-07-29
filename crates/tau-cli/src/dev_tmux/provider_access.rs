//! Provider-profile opt-in copying for the manual tmux E2E helper.

use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Path, PathBuf};

use tau_config::settings::{TauDirs, TestingSettings, load_testing_settings};
use tau_proto::ProviderName;

use super::{ensure_private_directory, reject_symlink};
use crate::CliError;

const TESTING_CONFIG_FILE: &str = "testing.yaml";
const PROVIDER_AUTH_DIR: &str = "auth.d";

/// Provider-access plan for one `tau dev tmux start` invocation.
pub(super) struct TestingProviderAccess {
    /// Real Tau state directory that may contain provider profiles.
    source_state_dir: Option<PathBuf>,
    /// Scratch Tau state directory where opted-in provider profiles are copied.
    scratch_state_dir: PathBuf,
    /// Semantic provider-access configuration loaded from `testing.yaml`.
    config: TestingProviderConfig,
}

/// Semantic state of testing provider access for the current start.
enum TestingProviderConfig {
    /// No `testing.yaml` was found, so the helper should warn and stay
    /// local-only.
    MissingConfig,
    /// `testing.yaml` was present and supplied this exact provider allowlist.
    Configured {
        /// Deduplicated exact provider profile names allowed for this start.
        allowed_profiles: BTreeSet<ProviderName>,
    },
}

impl TestingProviderAccess {
    /// Returns whether the tmux child should enable the provider extension.
    pub(super) fn provider_extension_enabled(&self) -> bool {
        !self.allowed_profiles().is_empty()
    }

    /// Copies exactly allowed provider auth profiles into scratch Tau state.
    pub(super) fn copy_allowed_profiles(&self) -> Result<(), CliError> {
        let allowed_profiles = self.allowed_profiles();
        let scratch_auth_dir = self.scratch_state_dir.join(PROVIDER_AUTH_DIR);
        reconcile_scratch_auth_dir(&scratch_auth_dir)?;
        if allowed_profiles.is_empty() {
            return Ok(());
        }
        let source_state_dir = self.source_state_dir.as_deref().ok_or_else(|| {
            CliError::Participant(
                "testing provider access is configured, but Tau could not determine the real state directory".to_owned(),
            )
        })?;
        let source_auth_dir = source_state_dir.join(PROVIDER_AUTH_DIR);
        reject_symlink_io(&source_auth_dir).map_err(|error| {
            CliError::Participant(format!(
                "refusing testing provider source auth directory `{}`: {error}",
                source_auth_dir.display()
            ))
        })?;
        if !source_auth_dir.is_dir() {
            return Err(CliError::Participant(format!(
                "testing provider access is configured, but provider auth directory `{}` does not exist",
                source_auth_dir.display()
            )));
        }
        ensure_private_directory(&scratch_auth_dir)?;

        for provider in allowed_profiles {
            if let Err(error) = copy_provider_profile(&source_auth_dir, &scratch_auth_dir, provider)
            {
                cleanup_allowed_scratch_profiles(&scratch_auth_dir, allowed_profiles);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Prints the provider-access outcome or safe local-only warning.
    pub(super) fn print_summary(&self) {
        match &self.config {
            TestingProviderConfig::MissingConfig => {
                eprintln!("{}", missing_testing_config_warning());
            }
            TestingProviderConfig::Configured { allowed_profiles }
                if allowed_profiles.is_empty() =>
            {
                eprintln!("{}", empty_testing_provider_warning());
            }
            TestingProviderConfig::Configured { allowed_profiles } => {
                let names = allowed_profiles
                    .iter()
                    .map(ProviderName::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                eprintln!(
                    "tau dev tmux: copied opted-in testing provider profile(s): {names}. \
Only these provider auth.d JSON files are available in the scratch Tau state."
                );
            }
        }
    }

    #[cfg(test)]
    fn is_missing_config(&self) -> bool {
        matches!(self.config, TestingProviderConfig::MissingConfig)
    }

    fn allowed_profiles(&self) -> &BTreeSet<ProviderName> {
        match &self.config {
            TestingProviderConfig::MissingConfig => empty_provider_set(),
            TestingProviderConfig::Configured { allowed_profiles } => allowed_profiles,
        }
    }
}

fn empty_provider_set() -> &'static BTreeSet<ProviderName> {
    static EMPTY: std::sync::OnceLock<BTreeSet<ProviderName>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(BTreeSet::new)
}

/// Loads testing provider access configuration for a tmux start invocation.
pub(super) fn prepare_provider_access(
    dirs: &TauDirs,
    scratch_state_dir: &Path,
) -> Result<TestingProviderAccess, CliError> {
    let settings = load_testing_settings(dirs).map_err(|error| {
        CliError::Participant(format!("{TESTING_CONFIG_FILE} failed to load:\n{error}"))
    })?;
    Ok(provider_access_from_settings(
        dirs.state_dir.clone(),
        scratch_state_dir.to_path_buf(),
        settings,
    ))
}

fn provider_access_from_settings(
    source_state_dir: Option<PathBuf>,
    scratch_state_dir: PathBuf,
    settings: Option<TestingSettings>,
) -> TestingProviderAccess {
    let config = match settings {
        Some(settings) => TestingProviderConfig::Configured {
            allowed_profiles: settings.testing_providers.into_iter().collect(),
        },
        None => TestingProviderConfig::MissingConfig,
    };
    TestingProviderAccess {
        source_state_dir,
        scratch_state_dir,
        config,
    }
}

fn missing_testing_config_warning() -> &'static str {
    "warning: tau dev tmux provider access is not configured; no real provider credentials, state, or config were copied into the scratch Tau environment. To opt in exact provider profiles for E2E testing, configure ~/.config/tau/testing.yaml and have the agent read the tau-self-knowledge-e2e-testing self-knowledge skill."
}

fn empty_testing_provider_warning() -> &'static str {
    "warning: tau dev tmux testing.yaml is configured with no testing_providers; no real provider credentials, state, or config were copied into the scratch Tau environment. To allow model access, add exact provider profile names and have the agent read the tau-self-knowledge-e2e-testing self-knowledge skill."
}

/// Reconciles scratch provider auth files with the current testing allowlist.
///
/// A helper-owned scratch root can be reused across starts, so provider secrets
/// copied by an earlier allowed run must not remain available when the current
/// `testing.yaml` is missing, empty, or narrower. The safest policy is to
/// remove every scratch `auth.d/*.json` before copying the current allowlist
/// again. Symlinked scratch auth paths or entries fail closed so cleanup never
/// follows attacker-controlled links.
fn reconcile_scratch_auth_dir(scratch_auth_dir: &Path) -> Result<(), CliError> {
    reject_symlink(scratch_auth_dir)?;
    if !scratch_auth_dir.exists() {
        return Ok(());
    }
    if !scratch_auth_dir.is_dir() {
        return Err(CliError::Participant(format!(
            "scratch provider auth path `{}` exists but is not a directory",
            scratch_auth_dir.display()
        )));
    }

    for entry in std::fs::read_dir(scratch_auth_dir)? {
        let entry = entry?;
        let path = entry.path();
        reject_symlink(&path)?;
        let metadata = entry.metadata()?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !file_name.ends_with(".json") {
            continue;
        }
        if metadata.is_dir() {
            return Err(CliError::Participant(format!(
                "refusing non-regular scratch provider profile path `{}`",
                path.display()
            )));
        }
        std::fs::remove_file(&path)?;
    }

    if std::fs::read_dir(scratch_auth_dir)?.next().is_none() {
        std::fs::remove_dir(scratch_auth_dir)?;
    }
    Ok(())
}

/// Removes current-allowlist scratch profiles after a failed copy attempt.
///
/// This keeps an aborted `tau dev tmux start` from leaving a partial set of
/// freshly copied credentials in a helper-marked scratch root that might later
/// be reused with provider access disabled.
fn cleanup_allowed_scratch_profiles(
    scratch_auth_dir: &Path,
    allowed_profiles: &BTreeSet<ProviderName>,
) {
    for provider in allowed_profiles {
        let path = scratch_auth_dir.join(format!("{provider}.json"));
        if reject_symlink(&path).is_ok() {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = std::fs::remove_file(path);
        }
    }
}

fn copy_provider_profile(
    source_auth_dir: &Path,
    scratch_auth_dir: &Path,
    provider: &ProviderName,
) -> Result<(), CliError> {
    let file_name = format!("{provider}.json");
    let source = source_auth_dir.join(&file_name);
    let destination = scratch_auth_dir.join(&file_name);
    copy_regular_private_file(&source, &destination).map_err(|source_error| {
        CliError::Participant(format!(
            "failed to copy opted-in testing provider profile `{provider}` from `{}` to `{}`: {source_error}",
            source.display(),
            destination.display()
        ))
    })
}

fn copy_regular_private_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    reject_symlink_io(source)?;
    let mut source_file = open_regular_file_no_follow(source)?;
    reject_symlink_io(destination)?;
    let parent = destination.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "destination has no parent")
    })?;
    ensure_private_directory(parent).map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut destination_file = open_private_file_no_follow(destination)?;
    std::io::copy(&mut source_file, &mut destination_file)?;
    Ok(())
}

fn reject_symlink_io(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::other(format!(
            "refusing symlink path `{}`",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn open_regular_file_no_follow(path: &Path) -> std::io::Result<File> {
    let file = open_file_no_follow_for_read(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other(format!(
            "refusing non-regular provider profile `{}`",
            path.display()
        )));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_file_no_follow_for_read(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_file_no_follow_for_read(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new().read(true).open(path)
}

#[cfg(unix)]
fn open_private_file_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_file_no_follow(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
}

#[cfg(test)]
mod tests;
