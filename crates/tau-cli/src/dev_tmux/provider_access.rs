//! Provider-profile opt-in copying for the manual tmux E2E helper.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::{fs as path_std_fs, io as path_std_io, sync as path_std_sync};

use tau_config::provider_settings::{
    MAX_PROVIDER_PROFILE_FILES, MAX_PROVIDER_PROFILE_SNAPSHOT_BYTES, ProviderCredential,
    ProviderProfileLeafSymlinkPolicy, parse_provider_credential, read_provider_profile,
};
use tau_config::settings::{
    TauDirs, TestingProvider, TestingSettings, extension_provider_config_dir_of,
    extension_provider_settings_dir_of, extension_secret_dir_of, load_testing_settings,
};

use super::{ensure_private_directory, reject_symlink};
use crate::CliError;

const TESTING_CONFIG_FILE: &str = "testing.yaml";
const PROVIDER_SETTINGS_DIR: &str = "providers";
const EXTENSION_SECRETS_DIR: &str = "secrets/ext";

/// Provider-access plan for one `tau dev tmux start` invocation.
pub(super) struct TestingProviderAccess {
    /// Real Tau state root used only as the copy source.
    source_state_dir: Option<PathBuf>,
    /// Real Tau config root used only as a profile copy source.
    source_config_dir: Option<PathBuf>,
    /// Private scratch Tau state root receiving exact copies.
    scratch_state_dir: PathBuf,
    /// Effective missing, empty, or populated allowlist.
    config: TestingProviderConfig,
}

/// Semantic state of testing provider access for the current start.
enum TestingProviderConfig {
    /// No testing configuration file was present.
    MissingConfig,
    /// The configuration explicitly selected these registrations.
    Configured {
        /// Exact extension/provider registration pairs.
        allowed_profiles: BTreeSet<TestingProvider>,
    },
}

impl TestingProviderAccess {
    /// Returns the exact provider extension instances the scratch Tau must
    /// start.
    pub(super) fn provider_extensions(&self) -> BTreeSet<tau_proto::ExtensionName> {
        self.allowed_profiles()
            .iter()
            .map(|target| target.extension.clone())
            .collect()
    }

    #[cfg(test)]
    /// Reports whether tests should observe at least one enabled provider.
    fn provider_extension_enabled(&self) -> bool {
        !self.allowed_profiles().is_empty()
    }

    /// Reconciles scratch state, then copies each exact instance/provider pair.
    pub(super) fn copy_allowed_profiles(&self) -> Result<(), CliError> {
        reconcile_scratch_tree(&self.scratch_state_dir.join(PROVIDER_SETTINGS_DIR))?;
        reconcile_scratch_tree(&self.scratch_state_dir.join(EXTENSION_SECRETS_DIR))?;
        let allowed = self.allowed_profiles();
        if allowed.is_empty() {
            return Ok(());
        }
        let mut instance_counts = BTreeMap::<&tau_proto::ExtensionName, usize>::new();
        for target in allowed {
            let count = instance_counts.entry(&target.extension).or_default();
            *count += 1;
            if MAX_PROVIDER_PROFILE_FILES < *count {
                return Err(CliError::Participant(format!(
                    "testing provider allowlist for instance '{}' exceeds \
                     {MAX_PROVIDER_PROFILE_FILES} profiles",
                    target.extension
                )));
            }
        }
        let source_state = self.source_state_dir.as_deref().ok_or_else(|| {
            CliError::Participant(
                "testing provider access is configured, but Tau could not determine the real state directory".to_owned(),
            )
        })?;
        let mut profile_bytes = BTreeMap::new();
        for target in allowed {
            if let Err(error) = copy_provider_target(
                self.source_config_dir.as_deref(),
                source_state,
                &self.scratch_state_dir,
                target,
                &mut profile_bytes,
            ) {
                let _ = reconcile_scratch_tree(&self.scratch_state_dir.join(PROVIDER_SETTINGS_DIR));
                let _ = reconcile_scratch_tree(&self.scratch_state_dir.join(EXTENSION_SECRETS_DIR));
                return Err(error);
            }
        }
        Ok(())
    }

    /// Prints the effective credential-copy boundary for the scratch session.
    pub(super) fn print_summary(&self) {
        match &self.config {
            TestingProviderConfig::MissingConfig => {
                eprintln!("{}", missing_testing_config_warning())
            }
            TestingProviderConfig::Configured { allowed_profiles }
                if allowed_profiles.is_empty() =>
            {
                eprintln!("{}", empty_testing_provider_warning());
            }
            TestingProviderConfig::Configured { allowed_profiles } => {
                let names = allowed_profiles
                    .iter()
                    .map(|target| format!("{}/{}", target.extension, target.provider))
                    .collect::<Vec<_>>()
                    .join(", ");
                eprintln!(
                    "tau dev tmux: copied opted-in testing provider registration(s): {names}. Only these settings and typed credential records are available in scratch state."
                );
            }
        }
    }

    #[cfg(test)]
    fn is_missing_config(&self) -> bool {
        matches!(self.config, TestingProviderConfig::MissingConfig)
    }

    fn allowed_profiles(&self) -> &BTreeSet<TestingProvider> {
        match &self.config {
            TestingProviderConfig::MissingConfig => empty_provider_set(),
            TestingProviderConfig::Configured { allowed_profiles } => allowed_profiles,
        }
    }
}

fn empty_provider_set() -> &'static BTreeSet<TestingProvider> {
    static EMPTY: path_std_sync::OnceLock<BTreeSet<TestingProvider>> =
        path_std_sync::OnceLock::new();
    EMPTY.get_or_init(BTreeSet::new)
}

/// Loads the testing allowlist without copying any provider material.
pub(super) fn prepare_provider_access(
    dirs: &TauDirs,
    scratch_state_dir: &Path,
) -> Result<TestingProviderAccess, CliError> {
    let settings = load_testing_settings(dirs).map_err(|error| {
        CliError::Participant(format!("{TESTING_CONFIG_FILE} failed to load:\n{error}"))
    })?;
    Ok(provider_access_from_dirs_and_settings(
        dirs.config_dir.clone(),
        dirs.state_dir.clone(),
        scratch_state_dir.to_path_buf(),
        settings,
    ))
}

#[cfg(test)]
fn provider_access_from_settings(
    source_state_dir: Option<PathBuf>,
    scratch_state_dir: PathBuf,
    settings: Option<TestingSettings>,
) -> TestingProviderAccess {
    provider_access_from_dirs_and_settings(None, source_state_dir, scratch_state_dir, settings)
}

fn provider_access_from_dirs_and_settings(
    source_config_dir: Option<PathBuf>,
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
        source_config_dir,
        scratch_state_dir,
        config,
    }
}

fn missing_testing_config_warning() -> &'static str {
    "warning: tau dev tmux provider access is not configured; no real provider credentials, state, or config were copied into scratch state. Configure ~/.config/tau/testing.yaml to opt in exact extension/provider pairs."
}

fn empty_testing_provider_warning() -> &'static str {
    "warning: tau dev tmux testing.yaml has no testing_providers; no real provider credentials, state, or config were copied into scratch state."
}

fn copy_provider_target(
    source_config: Option<&Path>,
    source_state: &Path,
    scratch_state: &Path,
    target: &TestingProvider,
    profile_bytes: &mut BTreeMap<tau_proto::ExtensionName, u64>,
) -> Result<(), CliError> {
    let state_settings =
        extension_provider_settings_dir_of(source_state, target.extension.as_str())
            .map_err(|error| CliError::Participant(error.to_string()))?
            .join(format!("{}.json", target.provider));
    let destination_settings =
        extension_provider_settings_dir_of(scratch_state, target.extension.as_str())
            .map_err(|error| CliError::Participant(error.to_string()))?
            .join(format!("{}.json", target.provider));
    let config_settings = source_config
        .map(|root| {
            extension_provider_config_dir_of(root, target.extension.as_str())
                .map(|path| path.join(format!("{}.json", target.provider)))
        })
        .transpose()
        .map_err(|error| CliError::Participant(error.to_string()))?;
    reject_path_components_no_follow(source_state, &state_settings).map_err(CliError::Io)?;
    reject_path_components_no_follow(scratch_state, &destination_settings).map_err(CliError::Io)?;
    let state_exists = path_entry_exists(&state_settings).map_err(CliError::Io)?;
    let config_exists = config_settings
        .as_deref()
        .map(path_entry_exists)
        .transpose()
        .map_err(CliError::Io)?
        .unwrap_or(false);
    if state_exists && config_exists {
        return Err(CliError::Participant(format!(
            "opted-in provider profile `{}/{}` is duplicated across config and state",
            target.extension, target.provider
        )));
    }
    let (source_settings, leaf_symlink_policy, source_label) =
        match (config_settings, state_exists, config_exists) {
            (Some(path), false, true) => {
                path.parent()
                    .expect("provider config path has instance parent")
                    .canonicalize()
                    .map_err(CliError::Io)?;
                let resolved = path.canonicalize().map_err(CliError::Io)?;
                if !resolved.is_file() {
                    return Err(CliError::Participant(
                        "testing provider config profile does not resolve to a regular file"
                            .to_owned(),
                    ));
                }
                (path, ProviderProfileLeafSymlinkPolicy::Follow, "config")
            }
            _ => (
                state_settings,
                ProviderProfileLeafSymlinkPolicy::Reject,
                "state",
            ),
        };
    let settings =
        read_provider_profile(&source_settings, leaf_symlink_policy).map_err(|error| {
            CliError::Participant(format!(
                "failed to read opted-in {} provider profile `{}/{}`: {error}",
                source_label, target.extension, target.provider
            ))
        })?;
    let instance_bytes = profile_bytes.entry(target.extension.clone()).or_default();
    *instance_bytes = instance_bytes.saturating_add(settings.len() as u64);
    if MAX_PROVIDER_PROFILE_SNAPSHOT_BYTES < *instance_bytes {
        return Err(CliError::Participant(format!(
            "testing provider profile snapshot for instance '{}' exceeds \
             {MAX_PROVIDER_PROFILE_SNAPSHOT_BYTES} bytes",
            target.extension
        )));
    }
    write_private_file(&destination_settings, &settings).map_err(|error| {
        CliError::Participant(format!(
            "failed to copy opted-in provider settings `{}/{}`: {error}",
            target.extension, target.provider
        ))
    })?;

    let value: serde_json::Value = serde_json::from_slice(&settings).map_err(|_| {
        CliError::Participant("opted-in provider profile is not valid JSON".to_owned())
    })?;
    let object = value.as_object().ok_or_else(|| {
        CliError::Participant("opted-in provider profile is not a JSON object".to_owned())
    })?;
    let credential = parse_provider_credential(&target.provider, object)
        .map_err(|error| CliError::Participant(error.to_string()))?;
    let ProviderCredential::Stored(reference) = credential else {
        return Ok(());
    };
    let source_secrets = extension_secret_dir_of(source_state, target.extension.as_str())
        .map_err(|error| CliError::Participant(error.to_string()))?
        .join("providers")
        .join(reference.identity().as_str());
    let destination_secrets = extension_secret_dir_of(scratch_state, target.extension.as_str())
        .map_err(|error| CliError::Participant(error.to_string()))?
        .join("providers")
        .join(reference.identity().as_str());
    reject_path_components_no_follow(source_state, &source_secrets).map_err(CliError::Io)?;
    reject_path_components_no_follow(scratch_state, &destination_secrets).map_err(CliError::Io)?;
    copy_regular_directory(&source_secrets, &destination_secrets).map_err(|error| {
        CliError::Participant(format!(
            "failed to copy opted-in provider credentials `{}/{}`: {error}",
            target.extension, target.provider
        ))
    })
}

fn path_entry_exists(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn reject_path_components_no_follow(root: &Path, target: &Path) -> path_std_io::Result<()> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| path_std_io::Error::other("provider path escapes its state root"))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        match path_std_fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(path_std_io::Error::other(
                    "provider path crosses a symlink component",
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(path_std_io::Error::other(
                    "provider path crosses a non-directory component",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        current.push(component.as_os_str());
    }
    reject_symlink_io(&current)
}

fn reconcile_scratch_tree(path: &Path) -> Result<(), CliError> {
    reject_symlink(path)?;
    if !path.exists() {
        return Ok(());
    }
    remove_tree_no_follow(path).map_err(CliError::Io)
}

fn remove_tree_no_follow(path: &Path) -> std::io::Result<()> {
    reject_symlink_io(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Err(path_std_io::Error::other(format!(
            "refusing non-directory scratch provider path `{}`",
            path.display()
        )));
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        reject_symlink_io(&child)?;
        if entry.file_type()?.is_dir() {
            remove_tree_no_follow(&child)?;
        } else {
            std::fs::remove_file(child)?;
        }
    }
    std::fs::remove_dir(path)
}

fn copy_regular_directory(source: &Path, destination: &Path) -> std::io::Result<()> {
    reject_symlink_io(source)?;
    if !std::fs::symlink_metadata(source)?.is_dir() {
        return Err(path_std_io::Error::other(
            "credential slot path is not a directory",
        ));
    }
    ensure_private_directory(destination)
        .map_err(|error| path_std_io::Error::other(error.to_string()))?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            return Err(path_std_io::Error::other(
                "nested credential directories are not supported",
            ));
        }
        copy_regular_private_file(&path, &destination.join(entry.file_name()))?;
    }
    Ok(())
}

fn copy_regular_private_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    reject_symlink_io(source)?;
    let mut source_file = open_regular_file_no_follow(source)?;
    prepare_private_file(destination).and_then(|mut destination_file| {
        std::io::copy(&mut source_file, &mut destination_file)?;
        destination_file.sync_all()
    })
}

fn write_private_file(destination: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut destination_file = prepare_private_file(destination)?;
    destination_file.write_all(contents)?;
    destination_file.sync_all()
}

fn prepare_private_file(destination: &Path) -> std::io::Result<File> {
    reject_symlink_io(destination)?;
    let parent = destination.parent().ok_or_else(|| {
        path_std_io::Error::new(
            path_std_io::ErrorKind::NotFound,
            "destination has no parent",
        )
    })?;
    ensure_private_directory(parent)
        .map_err(|error| path_std_io::Error::other(error.to_string()))?;
    open_private_file_no_follow(destination)
}

fn reject_symlink_io(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(path_std_io::Error::other(
            format!("refusing symlink path `{}`", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn open_regular_file_no_follow(path: &Path) -> std::io::Result<File> {
    let file = open_file_no_follow_for_read(path)?;
    if !file.metadata()?.is_file() {
        return Err(path_std_io::Error::other(format!(
            "refusing non-regular provider file `{}`",
            path.display()
        )));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_file_no_follow_for_read(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    path_std_fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_file_no_follow_for_read(path: &Path) -> std::io::Result<File> {
    path_std_fs::OpenOptions::new().read(true).open(path)
}

#[cfg(unix)]
fn open_private_file_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    path_std_fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_file_no_follow(path: &Path) -> std::io::Result<File> {
    path_std_fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
}

#[cfg(test)]
mod tests;
