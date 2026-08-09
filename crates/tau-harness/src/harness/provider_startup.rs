//! Coherent built-in provider settings snapshot and credential materialization.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::DirEntry;
use std::io;
use std::path::{Path, PathBuf};

use tau_config::provider_settings::{
    MAX_PROVIDER_PROFILE_FILES, MAX_PROVIDER_PROFILE_SNAPSHOT_BYTES, ProviderCredentialSlot,
    ProviderProfileLeafSymlinkPolicy, ProviderSettingsInstanceLock,
    parse_provider_credential_reference, read_provider_profile,
};
use tau_config::secret_sources::{SecretSourceError, SecretSources, resolve_declared_secret};
use tau_config::settings::BuiltinComponentIdentity;

use super::extension_data::{
    MAX_SECRET_DATA_FILE_BYTES, with_extension_data_scope_lock,
    write_extension_data_file_with_limit_locked,
};
use crate::error::HarnessError;
use crate::settings::{Config, ExtensionStartupDiagnostic, ExtensionStartupDiagnosticKind};

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

#[derive(Clone, Copy)]
enum ProfileSource {
    Config,
    State,
}

impl ProfileSource {
    fn label(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::State => "state",
        }
    }
}

fn load_extension_settings_files_at(
    root: &Path,
    source: ProfileSource,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let effective_root = match source {
        ProfileSource::Config => match root.canonicalize() {
            Ok(root) if root.is_dir() => root,
            Ok(_) => return Err("config profile root is not a directory".to_owned()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
            Err(error) => return Err(format!("could not resolve config profile root: {error}")),
        },
        ProfileSource::State => {
            match std::fs::symlink_metadata(root) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err("state profile root is not a real directory".to_owned());
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(Default::default());
                }
                Err(error) => {
                    return Err(format!(
                        "could not inspect state profile directory: {error}"
                    ));
                }
            }
            root.to_path_buf()
        }
    };
    let entries = match std::fs::read_dir(&effective_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => {
            return Err(format!(
                "could not list {} profile directory: {error}",
                source.label()
            ));
        }
    };
    let mut entries = entries
        .take(MAX_PROVIDER_PROFILE_FILES + 1)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("could not read {} profile entry: {error}", source.label()))?;
    if MAX_PROVIDER_PROFILE_FILES < entries.len() {
        return Err(format!(
            "{} profile directory exceeds {MAX_PROVIDER_PROFILE_FILES} entries",
            source.label()
        ));
    }
    entries.sort_by_key(DirEntry::file_name);
    let mut files = BTreeMap::new();
    let mut total_bytes = 0_u64;
    for entry in entries {
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "could not inspect {} profile entry: {error}",
                source.label()
            )
        })?;
        if matches!(source, ProfileSource::State)
            && (!file_type.is_file() || file_type.is_symlink())
        {
            return Err("state profile directory contains a non-regular entry".to_owned());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "settings file name is not UTF-8".to_owned())?;
        if !name.ends_with(".json")
            || tau_proto::ProviderName::try_new(name.trim_end_matches(".json").to_owned()).is_err()
        {
            return Err(format!(
                "{} profile file name is not a valid provider JSON name",
                source.label()
            ));
        }
        let path = entry.path();
        if matches!(source, ProfileSource::Config) {
            let resolved = path
                .canonicalize()
                .map_err(|error| format!("could not resolve config profile file: {error}"))?;
            if !resolved.is_file() {
                return Err("config profile file does not resolve to a regular file".to_owned());
            }
        }
        let leaf_symlink_policy = match source {
            ProfileSource::Config => ProviderProfileLeafSymlinkPolicy::Follow,
            ProfileSource::State => ProviderProfileLeafSymlinkPolicy::Reject,
        };
        let contents = read_provider_profile(&path, leaf_symlink_policy)
            .map_err(|error| format!("could not read {} profile file: {error}", source.label()))?;
        total_bytes = total_bytes.saturating_add(contents.len() as u64);
        if MAX_PROVIDER_PROFILE_SNAPSHOT_BYTES < total_bytes {
            return Err(format!(
                "{} profile snapshot exceeds {MAX_PROVIDER_PROFILE_SNAPSHOT_BYTES} bytes",
                source.label()
            ));
        }
        files.insert(name, contents);
    }
    Ok(files)
}

fn merged_settings_files(
    config_root: Option<PathBuf>,
    state_root: Option<&Path>,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let config = match config_root {
        Some(root) => load_extension_settings_files_at(&root, ProfileSource::Config)?,
        None => BTreeMap::new(),
    };
    let state = match state_root {
        Some(root) => load_extension_settings_files_at(root, ProfileSource::State)?,
        None => BTreeMap::new(),
    };
    for name in config.keys() {
        if state.contains_key(name) {
            return Err(format!(
                "provider profile '{}' is duplicated across config and state",
                name.trim_end_matches(".json")
            ));
        }
    }
    if MAX_PROVIDER_PROFILE_FILES < config.len() + state.len() {
        return Err(format!(
            "merged provider snapshot exceeds {MAX_PROVIDER_PROFILE_FILES} files"
        ));
    }
    let total = config
        .values()
        .chain(state.values())
        .fold(0_u64, |total, bytes| {
            total.saturating_add(bytes.len() as u64)
        });
    if MAX_PROVIDER_PROFILE_SNAPSHOT_BYTES < total {
        return Err(format!(
            "merged provider snapshot exceeds {MAX_PROVIDER_PROFILE_SNAPSHOT_BYTES} bytes"
        ));
    }
    let mut merged = config;
    merged.extend(state);
    Ok(merged)
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
    config_dir: Option<&Path>,
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
            match ProviderSettingsInstanceLock::acquire_or_create(state_dir, &extension.name) {
                Ok(lock) => Some(lock),
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
        let config_root = config_dir
            .map(|root| {
                tau_config::settings::extension_provider_config_dir_of(root, &extension.name)
            })
            .transpose()
            .map_err(|error| HarnessError::Participant(error.to_string()))?;
        let settings_files = match merged_settings_files(
            config_root,
            settings_lock
                .as_ref()
                .map(ProviderSettingsInstanceLock::root),
        ) {
            Ok(files) => files,
            Err(error) if extension.require => {
                return Err(HarnessError::Participant(format!(
                    "provider extension '{}' profile discovery failed: {error}",
                    extension.name
                )));
            }
            Err(error) => {
                diagnostics.push(ExtensionStartupDiagnostic {
                    extension: extension.name.clone(),
                    message: format!(
                        "optional provider extension '{}' profile discovery failed: {error}",
                        extension.name,
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

/// Snapshot credential-free provider settings without resolving or
/// materializing credentials for a memory-only preview.
///
/// Provider settings determine the advertised model metadata and are therefore
/// required to render the same prompt and tool surface as an ordinary harness.
/// The preview still binds every declared provider secret so the ordinary
/// extension-secret path cannot expose credential values.
pub(super) fn snapshot_memory_only_provider_settings(
    config: &Config,
    config_dir: Option<&Path>,
    state_dir: &Path,
) -> Result<ProviderStartupSnapshot, HarnessError> {
    let mut snapshot = ProviderStartupSnapshot::default();
    for extension in config
        .extensions
        .values()
        .filter(|extension| extension.role.as_deref() == Some("provider"))
    {
        if extension.component == Some(BuiltinComponentIdentity::Provider) {
            snapshot.bound_names.insert(
                extension.name.clone(),
                extension.secrets.keys().cloned().collect(),
            );
        }
        let settings_lock =
            match ProviderSettingsInstanceLock::acquire_existing(state_dir, &extension.name) {
                Ok(lock) => lock,
                Err(error) if extension.require => return Err(error.into()),
                Err(_) => {
                    snapshot.diagnostics.push(ExtensionStartupDiagnostic {
                        extension: extension.name.clone(),
                        message: format!(
                            "optional provider extension '{}' did not initialize",
                            extension.name
                        ),
                        kind: ExtensionStartupDiagnosticKind::OptionalSkip,
                    });
                    snapshot.skipped_extensions.insert(extension.name.clone());
                    continue;
                }
            };
        let config_root = config_dir
            .map(|root| {
                tau_config::settings::extension_provider_config_dir_of(root, &extension.name)
            })
            .transpose()
            .map_err(|error| HarnessError::Participant(error.to_string()))?;
        let settings_files = merged_settings_files(
            config_root,
            settings_lock
                .as_ref()
                .map(ProviderSettingsInstanceLock::root),
        );
        match settings_files {
            Ok(settings_files) => {
                snapshot
                    .settings
                    .insert(extension.name.clone(), settings_files);
            }
            Err(error) if extension.require => {
                return Err(HarnessError::Participant(format!(
                    "provider extension '{}' profile discovery failed: {error}",
                    extension.name
                )));
            }
            Err(error) => {
                snapshot.diagnostics.push(ExtensionStartupDiagnostic {
                    extension: extension.name.clone(),
                    message: format!(
                        "optional provider extension '{}' profile discovery failed: {error}",
                        extension.name,
                    ),
                    kind: ExtensionStartupDiagnosticKind::OptionalSkip,
                });
                snapshot.skipped_extensions.insert(extension.name.clone());
            }
        }
    }
    Ok(snapshot)
}
