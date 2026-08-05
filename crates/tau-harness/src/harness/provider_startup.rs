//! Coherent built-in provider settings snapshot and credential materialization.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _};
use std::path::Path;

use tau_config::provider_settings::{
    ProviderCredentialSlot, ProviderSettingsInstanceLock, parse_provider_credential_reference,
};
use tau_config::secret_sources::{SecretSourceError, SecretSources, resolve_declared_secret};
use tau_config::settings::BuiltinComponentIdentity;

use super::extension_data::{
    MAX_SECRET_DATA_FILE_BYTES, with_extension_data_scope_lock,
    write_extension_data_file_with_limit_locked,
};
use crate::error::HarnessError;
use crate::settings::{Config, ExtensionStartupDiagnostic, ExtensionStartupDiagnosticKind};

/// Maximum number of CLI-owned settings files sent to one extension at startup.
const MAX_EXTENSION_SETTINGS_FILES: usize = 4_096;
/// Maximum bytes accepted from one CLI-owned settings file.
const MAX_EXTENSION_SETTINGS_FILE_BYTES: u64 = 1024 * 1024;
/// Maximum total settings bytes, reserving one MiB for the Configure envelope.
const MAX_EXTENSION_SETTINGS_SNAPSHOT_BYTES: u64 =
    tau_proto::MAX_PROTOCOL_MESSAGE_BYTES - MAX_EXTENSION_SETTINGS_FILE_BYTES;

/// Coherent provider settings, bound declarations, and startup diagnostics.
#[derive(Debug, Default)]
pub(super) struct ProviderStartupSnapshot {
    /// Exact settings bytes retained for each provider extension's Configure.
    pub(super) settings: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    /// Named declarations consumed by provider credential materialization.
    pub(super) bound_names: BTreeMap<String, BTreeSet<String>>,
    /// Redacted extension-visible failures discovered during materialization.
    pub(super) diagnostics: Vec<ExtensionStartupDiagnostic>,
    /// Optional provider extensions skipped after snapshot/materialization
    /// errors.
    pub(super) skipped_extensions: BTreeSet<String>,
}

fn load_extension_settings_files_at(root: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err("settings root is not a real directory".to_owned());
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(format!("could not inspect settings directory: {error}")),
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(format!("could not list settings directory: {error}")),
    };
    let mut files = BTreeMap::new();
    let mut total_bytes = 0_u64;
    for (index, entry) in entries.enumerate() {
        if MAX_EXTENSION_SETTINGS_FILES <= index {
            return Err(format!(
                "settings directory exceeds {MAX_EXTENSION_SETTINGS_FILES} entries"
            ));
        }
        let entry = entry.map_err(|error| format!("could not read settings entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("could not inspect settings entry: {error}"))?;
        if !file_type.is_file() || file_type.is_symlink() {
            return Err("settings directory contains a non-regular entry".to_owned());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "settings file name is not UTF-8".to_owned())?;
        if !name.ends_with(".json")
            || tau_proto::ProviderName::try_new(name.trim_end_matches(".json").to_owned()).is_err()
        {
            return Err("settings file name is not a valid provider JSON name".to_owned());
        }
        let mut contents = Vec::new();
        open_settings_file_no_follow(&entry.path())
            .and_then(|file| {
                file.take(MAX_EXTENSION_SETTINGS_FILE_BYTES + 1)
                    .read_to_end(&mut contents)
            })
            .map_err(|error| format!("could not read settings file: {error}"))?;
        if MAX_EXTENSION_SETTINGS_FILE_BYTES < contents.len() as u64 {
            return Err(format!(
                "settings file exceeds {MAX_EXTENSION_SETTINGS_FILE_BYTES} bytes"
            ));
        }
        total_bytes = total_bytes.saturating_add(contents.len() as u64);
        if MAX_EXTENSION_SETTINGS_SNAPSHOT_BYTES < total_bytes {
            return Err(format!(
                "settings snapshot exceeds {MAX_EXTENSION_SETTINGS_SNAPSHOT_BYTES} bytes"
            ));
        }
        files.insert(name, contents);
    }
    Ok(files)
}

#[cfg(unix)]
fn open_settings_file_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_settings_file_no_follow(path: &Path) -> io::Result<File> {
    File::open(path)
}
/// Capture each built-in provider instance's settings generation and
/// publish every named API-key binding selected by that exact
/// generation.
///
/// Named-source settings are the only settings allowed to replace a record:
/// direct-entry records have no source object and remain untouched.  An
/// unavailable source overwrites its old materialization with an empty
/// typed record, so a stale key cannot activate the profile after
/// restart.
pub(super) fn snapshot_and_materialize_named_provider_credentials(
    config: &Config,
    state_dir: &Path,
    secret_sources: &SecretSources,
) -> Result<ProviderStartupSnapshot, HarnessError> {
    let mut snapshots = BTreeMap::new();
    let mut bound_names = BTreeMap::<String, BTreeSet<String>>::new();
    let mut diagnostics = Vec::new();
    let mut skipped_extensions = BTreeSet::new();
    for extension in config
        .extensions
        .values()
        .filter(|extension| extension.role.as_deref() == Some("provider"))
    {
        let settings_lock =
            match ProviderSettingsInstanceLock::acquire_existing(state_dir, &extension.name) {
                Ok(lock) => lock,
                Err(error) if extension.require => return Err(error.into()),
                Err(_) => {
                    diagnostics.push(ExtensionStartupDiagnostic {
                        extension: extension.name.clone(),
                        message: format!(
                            "optional provider extension '{}' did not initialize",
                            extension.name
                        ),
                        kind: ExtensionStartupDiagnosticKind::OptionalSkip,
                    });
                    skipped_extensions.insert(extension.name.clone());
                    continue;
                }
            };
        let Some(settings_lock) = settings_lock else {
            snapshots.insert(extension.name.clone(), BTreeMap::new());
            continue;
        };
        let settings_files = match load_extension_settings_files_at(settings_lock.root()) {
            Ok(files) => files,
            Err(error) if extension.require => {
                return Err(HarnessError::Participant(error));
            }
            Err(_) => {
                diagnostics.push(ExtensionStartupDiagnostic {
                    extension: extension.name.clone(),
                    message: format!(
                        "optional provider extension '{}' did not initialize",
                        extension.name
                    ),
                    kind: ExtensionStartupDiagnosticKind::OptionalSkip,
                });
                skipped_extensions.insert(extension.name.clone());
                continue;
            }
        };
        if extension.component != Some(BuiltinComponentIdentity::Provider) {
            snapshots.insert(extension.name.clone(), settings_files);
            continue;
        }
        let secret_root = tau_config::settings::extension_secret_dir_of(state_dir, &extension.name)
            .map_err(|error| HarnessError::Participant(error.to_string()))?;
        let mut publications = Vec::new();
        let mut materialization_failed = false;
        for (file_name, contents) in &settings_files {
            let profile_name = file_name.trim_end_matches(".json");
            let profile = tau_proto::ProviderName::try_new(profile_name.to_owned())
                .expect("bounded settings loader validates provider file names");
            let Ok(settings) = serde_json::from_slice::<serde_json::Value>(contents) else {
                continue;
            };
            let Some(settings) = settings.as_object() else {
                continue;
            };
            let Ok(reference) = parse_provider_credential_reference(&profile, settings) else {
                continue;
            };
            let Some(source_name) = reference.named_source().map(str::to_owned) else {
                continue;
            };
            if reference.slot() != ProviderCredentialSlot::ApiKey {
                continue;
            }
            bound_names
                .entry(extension.name.clone())
                .or_default()
                .insert(source_name.clone());
            let resolution = match extension.secrets.get(&source_name) {
                Some(declaration) => resolve_declared_secret(
                    state_dir,
                    secret_sources,
                    &extension.name,
                    &source_name,
                    declaration,
                ),
                None => Ok(None),
            };
            let value = match resolution {
                Ok(Some(value)) => value.expose_secret().to_owned(),
                Ok(None) | Err(SecretSourceError::MissingRequired { .. }) => String::new(),
                Err(_) if extension.require => {
                    return Err(HarnessError::Participant(format!(
                        "provider extension '{}' could not resolve a named secret",
                        extension.name
                    )));
                }
                Err(_) => {
                    materialization_failed = true;
                    break;
                }
            };
            let record = serde_json::to_vec(&serde_json::json!({
                "version": 0, "kind": "api_key", "value": value,
            }))
            .map_err(|error| HarnessError::Participant(error.to_string()))?;
            publications.push((reference.path().as_str().to_owned(), record));
            if value.is_empty() {
                diagnostics.push(ExtensionStartupDiagnostic {
                    extension: extension.name.clone(),
                    message: format!(
                        "provider profile '{profile}' for extension '{}' is disabled: configured named secret '{source_name}' is unavailable",
                        extension.name
                    ),
                    kind: ExtensionStartupDiagnosticKind::OptionalSkip,
                });
            }
        }
        if materialization_failed {
            diagnostics.push(ExtensionStartupDiagnostic {
                extension: extension.name.clone(),
                message: format!(
                    "optional provider extension '{}' did not initialize",
                    extension.name
                ),
                kind: ExtensionStartupDiagnosticKind::OptionalSkip,
            });
            skipped_extensions.insert(extension.name.clone());
            continue;
        }
        if !publications.is_empty() {
            let publication = with_extension_data_scope_lock(&secret_root, || {
                for (path, record) in publications {
                    write_extension_data_file_with_limit_locked(
                        &secret_root,
                        path,
                        record,
                        MAX_SECRET_DATA_FILE_BYTES,
                    )?;
                }
                Ok(())
            });
            match publication {
                Ok(()) => {}
                Err(error) if extension.require => {
                    return Err(HarnessError::Participant(error.message));
                }
                Err(_) => {
                    diagnostics.push(ExtensionStartupDiagnostic {
                        extension: extension.name.clone(),
                        message: format!(
                            "optional provider extension '{}' did not initialize",
                            extension.name
                        ),
                        kind: ExtensionStartupDiagnosticKind::OptionalSkip,
                    });
                    skipped_extensions.insert(extension.name.clone());
                    continue;
                }
            }
        }
        snapshots.insert(extension.name.clone(), settings_files);
    }
    Ok(ProviderStartupSnapshot {
        settings: snapshots,
        bound_names,
        diagnostics,
        skipped_extensions,
    })
}

/// Suppress built-in provider declarations when persistent settings are
/// intentionally unavailable in memory-only mode.
pub(super) fn memory_only_provider_bound_names(
    config: &Config,
) -> BTreeMap<String, BTreeSet<String>> {
    config
        .extensions
        .values()
        .filter(|extension| extension.component == Some(BuiltinComponentIdentity::Provider))
        .map(|extension| {
            (
                extension.name.clone(),
                extension.secrets.keys().cloned().collect(),
            )
        })
        .collect()
}
