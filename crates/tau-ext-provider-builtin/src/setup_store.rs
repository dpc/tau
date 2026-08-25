//! CLI-owned provider registration storage.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::{fs as path_std_fs, io as path_std_io};

use tau_config::provider_settings::{
    MAX_PROVIDER_PROFILE_FILES, MAX_PROVIDER_PROFILE_SNAPSHOT_BYTES, ProviderCredential,
    ProviderCredentialIdentity, ProviderCredentialSlot, ProviderProfileLeafSymlinkPolicy,
    ProviderSettingsInstanceLock, ProviderSettingsLockAttempt, parse_provider_credential,
    read_provider_profile,
};
use tau_config::secret_sources::{
    EnvironmentDisposition, MAX_SECRET_DATA_FILE_BYTES, load_secret_sources,
    resolve_declared_secret,
};
use tau_config::settings::TauDirs;

use crate::credential_record::ApiKeyCredential;

/// One complete provider registration publication.
pub(crate) struct ProviderSetupPlan {
    /// Stable configured provider-extension instance name.
    pub(crate) extension_instance: tau_proto::ExtensionName,
    /// Provider namespace used by settings and secret paths.
    pub(crate) provider: tau_proto::ProviderName,
    /// Credential-free provider settings JSON.
    pub(crate) settings: Vec<u8>,
    /// Exhaustive keyless or stored credential publication selection.
    pub(crate) credential: CredentialSetup,
}

/// Credential state published atomically before one provider profile.
pub(crate) enum CredentialSetup {
    /// Publish settings without creating or changing Secret state.
    Keyless,
    /// Publish one typed Secret record before activating settings.
    Stored {
        /// Complete typed credential record and canonical destination.
        secret: SecretWrite,
        /// Configured named source resolved inside the instance transaction.
        named_source: Option<NamedSecretSource>,
    },
    /// Activate a stored-credential profile without publishing a credential.
    ///
    /// A later persistent harness startup may materialize the named binding
    /// recorded in settings after the matching declaration and value exist.
    DeferredNamed {
        /// Canonical future credential path used for replacement cleanup.
        path: tau_proto::ExtensionDataPath,
    },
}

/// Destination selected for a provider profile created by the CLI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProfileTarget {
    /// Mutable host-local state.
    State,
    /// Portable user configuration.
    Config,
    /// Standard output for a dotfiles workflow.
    Stdout,
}

/// Durable source that owns an existing provider profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProfileSource {
    /// Portable user configuration.
    Config,
    /// Mutable host-local state.
    State,
}

impl ProfileSource {
    /// Returns the stable CLI source label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::State => "state",
        }
    }
}

/// One exact configured declaration selected by the setup picker.
#[derive(Clone)]
pub(crate) struct NamedSecretSource {
    /// Declared source name serialized into credential-free settings.
    pub(crate) name: String,
    /// Exact targeted-extension declaration controlling optionality.
    pub(crate) declaration: tau_config::settings::ExtensionSecretEntry,
}

/// One opaque complete secret write.
pub(crate) struct SecretWrite {
    /// Relative path inside the configured instance's Secret scope.
    pub(crate) path: tau_proto::ExtensionDataPath,
    /// Serialized typed credential bytes.
    pub(crate) contents: SecretBytes,
}

/// Secret bytes whose debug representation never exposes their payload.
pub(crate) struct SecretBytes(
    /// Complete serialized typed credential record.
    Vec<u8>,
);

/// Coherent settings and credential bytes captured under lifecycle locks.
pub(crate) struct SetupSnapshot {
    /// Complete profiles in deterministic provider order.
    pub(crate) profiles: Vec<SetupProfile>,
    /// Existing closed credential slots keyed by opaque profile identity and
    /// family.
    pub(crate) credentials: BTreeMap<(ProviderCredentialIdentity, ProviderCredentialSlot), Vec<u8>>,
}

/// One credential-free provider profile discovered by setup tooling.
pub(crate) struct SetupProfile {
    /// Validated provider namespace.
    pub(crate) provider: tau_proto::ProviderName,
    /// Durable layer that owns the profile.
    pub(crate) source: ProfileSource,
    /// User-visible host path.
    pub(crate) path: PathBuf,
    /// Exact credential-free JSON bytes.
    pub(crate) contents: Vec<u8>,
}

impl SecretBytes {
    /// Wraps serialized secret bytes.
    pub(crate) fn new(contents: Vec<u8>) -> Self {
        Self(contents)
    }

    /// Borrows the serialized secret payload for a private filesystem write.
    fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SecretBytes")
            .field("len", &self.0.len())
            .finish()
    }
}

/// Harness-layout-aware storage used only by provider setup commands.
pub(crate) struct SetupStore {
    /// Tau user-configuration root.
    config_dir: PathBuf,
    /// Tau user-state root.
    state_dir: PathBuf,
    /// Maximum number of profile files accepted across config and state.
    max_profile_files: usize,
    /// Test-only signal after a nonblocking lock attempt confirms contention.
    #[cfg(test)]
    contention: Option<std::sync::Arc<std::sync::Barrier>>,
    /// Test-only barriers that pause while holding the instance lock.
    #[cfg(test)]
    acquired: Option<(
        std::sync::Arc<std::sync::Barrier>,
        std::sync::Arc<std::sync::Barrier>,
    )>,
}

impl SetupStore {
    /// Opens the default user-state setup store.
    pub(crate) fn open_default() -> path_std_io::Result<Self> {
        let dirs = TauDirs::default();
        let config_dir = dirs.config_dir.ok_or_else(|| {
            path_std_io::Error::new(
                path_std_io::ErrorKind::NotFound,
                "cannot determine Tau config directory",
            )
        })?;
        let state_dir = dirs.state_dir.ok_or_else(|| {
            path_std_io::Error::new(
                path_std_io::ErrorKind::NotFound,
                "cannot determine Tau state directory",
            )
        })?;
        Ok(Self {
            config_dir,
            state_dir,
            max_profile_files: MAX_PROVIDER_PROFILE_FILES,
            #[cfg(test)]
            contention: None,
            #[cfg(test)]
            acquired: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn open_in(state_dir: impl Into<PathBuf>) -> Self {
        let state_dir = state_dir.into();
        Self {
            config_dir: state_dir.join("config"),
            state_dir,
            max_profile_files: MAX_PROVIDER_PROFILE_FILES,
            contention: None,
            acquired: None,
        }
    }

    /// Overrides the profile-file limit for focused boundary tests.
    #[cfg(test)]
    pub(crate) fn with_max_profile_files(mut self, max_profile_files: usize) -> Self {
        self.max_profile_files = max_profile_files;
        self
    }

    #[cfg(test)]
    fn with_contention(mut self, barrier: std::sync::Arc<std::sync::Barrier>) -> Self {
        self.contention = Some(barrier);
        self
    }

    #[cfg(test)]
    fn with_acquired_pause(
        mut self,
        entered: std::sync::Arc<std::sync::Barrier>,
        release: std::sync::Arc<std::sync::Barrier>,
    ) -> Self {
        self.acquired = Some((entered, release));
        self
    }

    fn acquire_instance_lock(
        &self,
        extension_instance: &tau_proto::ExtensionName,
    ) -> path_std_io::Result<Option<ProviderSettingsInstanceLock>> {
        let lock = match ProviderSettingsInstanceLock::try_acquire_existing(
            &self.state_dir,
            extension_instance.as_str(),
        )? {
            ProviderSettingsLockAttempt::Missing => None,
            ProviderSettingsLockAttempt::Acquired(lock) => Some(lock),
            ProviderSettingsLockAttempt::Contended => {
                #[cfg(test)]
                if let Some(barrier) = &self.contention {
                    barrier.wait();
                }
                ProviderSettingsInstanceLock::acquire_existing(
                    &self.state_dir,
                    extension_instance.as_str(),
                )?
            }
        };
        #[cfg(test)]
        if lock.is_some()
            && let Some((entered, release)) = &self.acquired
        {
            entered.wait();
            release.wait();
        }
        Ok(lock)
    }

    /// Applies secret-first and settings-last, making the settings write the
    /// registration activation point.
    #[cfg(test)]
    pub(crate) fn apply(&self, plan: &ProviderSetupPlan) -> path_std_io::Result<Option<PathBuf>> {
        self.apply_to(plan, ProfileTarget::State)
    }

    /// Applies a setup plan to the explicitly selected profile target.
    pub(crate) fn apply_to(
        &self,
        plan: &ProviderSetupPlan,
        target: ProfileTarget,
    ) -> path_std_io::Result<Option<PathBuf>> {
        let settings_root = tau_config::settings::extension_provider_settings_dir_of(
            &self.state_dir,
            plan.extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        ensure_private_directory_tree(
            &self.state_dir,
            &PathBuf::from("providers").join(plan.extension_instance.as_str()),
        )?;
        // Serialize setup with harness startup: settings generation decides which
        // named source may materialize, so this lock must precede Secret scope.
        let _settings_lock = self
            .acquire_instance_lock(&plan.extension_instance)?
            .ok_or_else(|| {
                path_std_io::Error::new(
                    path_std_io::ErrorKind::NotFound,
                    "provider settings directory disappeared before locking",
                )
            })?;
        let settings_rel = PathBuf::from(format!("{}.json", plan.provider));
        let config_root = tau_config::settings::extension_provider_config_dir_of(
            &self.config_dir,
            plan.extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        let state_path = settings_root.join(&settings_rel);
        let config_path = config_root.join(&settings_rel);
        let conflicting = match target {
            ProfileTarget::State => &config_path,
            ProfileTarget::Config | ProfileTarget::Stdout => &state_path,
        };
        if conflicting.exists() {
            return Err(path_std_io::Error::new(
                path_std_io::ErrorKind::AlreadyExists,
                format!(
                    "provider '{}' already exists in the other profile source",
                    plan.provider
                ),
            ));
        }
        if target == ProfileTarget::Stdout && path_exists(&config_path)? {
            return Err(path_std_io::Error::new(
                path_std_io::ErrorKind::AlreadyExists,
                "provider profile already exists in config; stdout output cannot replace its credential identity",
            ));
        }
        let target_path = match target {
            ProfileTarget::State => Some(&state_path),
            ProfileTarget::Config => Some(&config_path),
            ProfileTarget::Stdout => None,
        };
        let replaced_credential = match target_path {
            Some(path) if path_exists(path)? => {
                let contents = read_provider_profile(
                    path,
                    match target {
                        ProfileTarget::State => ProviderProfileLeafSymlinkPolicy::Reject,
                        ProfileTarget::Config => ProviderProfileLeafSymlinkPolicy::Follow,
                        ProfileTarget::Stdout => unreachable!("stdout has no profile path"),
                    },
                );
                contents.ok().and_then(|contents| {
                    credential_from_settings(&SetupProfile {
                        provider: plan.provider.clone(),
                        source: match target {
                            ProfileTarget::State => ProfileSource::State,
                            ProfileTarget::Config => ProfileSource::Config,
                            ProfileTarget::Stdout => unreachable!("stdout has no profile path"),
                        },
                        path: path.to_path_buf(),
                        contents,
                    })
                    .ok()
                })
            }
            Some(_) | None => None,
        };
        let (secret, named_source, deferred_path) = match &plan.credential {
            CredentialSetup::Keyless => (None, None, None),
            CredentialSetup::Stored {
                secret,
                named_source,
            } => (Some(secret), named_source.as_ref(), None),
            CredentialSetup::DeferredNamed { path } => (None, None, Some(path)),
        };
        let named_contents = named_source
            .map(|source| self.materialize_named_secret(&plan.extension_instance, source))
            .transpose()?;
        if let Some(secret) = secret {
            self.write_secret(
                &plan.extension_instance,
                secret,
                named_contents
                    .as_deref()
                    .unwrap_or_else(|| secret.contents.expose()),
            )?;
        }
        let path = match target {
            ProfileTarget::State => {
                reject_existing_symlink_components(&settings_root, &settings_rel)?;
                atomic_private_write(&state_path, &plan.settings)?;
                Some(state_path)
            }
            ProfileTarget::Config => {
                path_std_fs::create_dir_all(&config_root)?;
                atomic_write(&config_path, &plan.settings)?;
                Some(config_path)
            }
            ProfileTarget::Stdout => None,
        };
        if let Some(ProviderCredential::Stored(reference)) = replaced_credential
            && (deferred_path.is_some()
                || secret.is_none_or(|new_secret| new_secret.path != *reference.path()))
        {
            self.remove_credential(&plan.extension_instance, &reference)?;
        }
        Ok(path)
    }

    /// Retires one superseded closed credential record after its replacement
    /// profile becomes active.
    fn remove_credential(
        &self,
        extension_instance: &tau_proto::ExtensionName,
        reference: &tau_config::provider_settings::ProviderCredentialReference,
    ) -> path_std_io::Result<()> {
        use fs2::FileExt as _;

        let secret_root = tau_config::settings::extension_secret_dir_of(
            &self.state_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        let secret_lock = match open_directory_no_follow(&secret_root) {
            Ok(lock) => {
                lock.lock_exclusive()?;
                Some(lock)
            }
            Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let relative = PathBuf::from(reference.path().as_str());
        reject_existing_symlink_components(&secret_root, &relative)?;
        let result = remove_file_sync(&secret_root.join(relative));
        if let Some(secret_lock) = secret_lock {
            fs2::FileExt::unlock(&secret_lock)?;
        }
        match result {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Publishes one credential for an unchanged existing profile without
    /// creating, replacing, or removing either profile source.
    pub(crate) fn publish_credential(
        &self,
        extension_instance: &tau_proto::ExtensionName,
        provider: &tau_proto::ProviderName,
        expected_source: ProfileSource,
        expected_settings: &[u8],
        secret: &SecretWrite,
        named_source: Option<&NamedSecretSource>,
    ) -> path_std_io::Result<()> {
        ensure_private_directory_tree(
            &self.state_dir,
            &PathBuf::from("providers").join(extension_instance.as_str()),
        )?;
        let _settings_lock = self
            .acquire_instance_lock(extension_instance)?
            .ok_or_else(|| {
                path_std_io::Error::new(
                    path_std_io::ErrorKind::NotFound,
                    "provider settings directory disappeared before locking",
                )
            })?;
        let matching = self
            .settings_files_unlocked(extension_instance)?
            .into_iter()
            .filter(|profile| profile.provider == *provider)
            .collect::<Vec<_>>();
        let [profile] = matching.as_slice() else {
            return Err(path_std_io::Error::new(
                path_std_io::ErrorKind::AlreadyExists,
                "provider profile is missing or duplicated across config and state",
            ));
        };
        if profile.source != expected_source || profile.contents != expected_settings {
            return Err(path_std_io::Error::new(
                path_std_io::ErrorKind::InvalidData,
                "provider profile changed while authentication was in progress",
            ));
        }
        let named_contents = named_source
            .map(|source| self.materialize_named_secret(extension_instance, source))
            .transpose()?;
        self.write_secret(
            extension_instance,
            secret,
            named_contents
                .as_deref()
                .unwrap_or_else(|| secret.contents.expose()),
        )
    }

    /// Resolves one declared named source and serializes its canonical typed
    /// API-key record without exposing the value to callers.
    fn materialize_named_secret(
        &self,
        extension_instance: &tau_proto::ExtensionName,
        source: &NamedSecretSource,
    ) -> path_std_io::Result<Vec<u8>> {
        let sources = load_secret_sources(EnvironmentDisposition::Retain)
            .map_err(path_std_io::Error::other)?;
        let value = resolve_declared_secret(
            &self.state_dir,
            &sources,
            extension_instance.as_str(),
            &source.name,
            &source.declaration,
        )
        .map_err(path_std_io::Error::other)?
        .ok_or_else(|| {
            path_std_io::Error::new(
                path_std_io::ErrorKind::NotFound,
                format!("configured named secret '{}' is unavailable", source.name),
            )
        })?;
        serde_json::to_vec(&ApiKeyCredential::new(value.expose_secret().to_owned()))
            .map_err(path_std_io::Error::other)
    }

    /// Writes one Secret record while preserving the no-symlink private-tree
    /// contract shared by setup and login.
    fn write_secret(
        &self,
        extension_instance: &tau_proto::ExtensionName,
        secret: &SecretWrite,
        contents: &[u8],
    ) -> path_std_io::Result<()> {
        use fs2::FileExt as _;

        if MAX_SECRET_DATA_FILE_BYTES < u64::try_from(contents.len()).unwrap_or(u64::MAX) {
            return Err(path_std_io::Error::new(
                path_std_io::ErrorKind::InvalidData,
                format!(
                    "credential record exceeds the {MAX_SECRET_DATA_FILE_BYTES}-byte Secret limit"
                ),
            ));
        }
        let secret_root = tau_config::settings::extension_secret_dir_of(
            &self.state_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        ensure_private_directory_tree(
            &self.state_dir,
            &PathBuf::from("secrets/ext").join(extension_instance.as_str()),
        )?;
        let secret_lock = open_directory_no_follow(&secret_root)?;
        secret_lock.lock_exclusive()?;
        let secret_rel = sanitize_secret_path(secret.path.as_str())?;
        reject_existing_symlink_components(&secret_root, &secret_rel)?;
        if let Some(parent) = secret_rel.parent() {
            ensure_private_directory_tree(&secret_root, parent)?;
        }
        atomic_private_write(&secret_root.join(secret_rel), contents)?;
        fs2::FileExt::unlock(&secret_lock)
    }

    /// Removes settings first and then every credential slot used by
    /// version-zero built-in providers.
    #[cfg(test)]
    pub(crate) fn remove(
        &self,
        extension_instance: &tau_proto::ExtensionName,
        provider: &tau_proto::ProviderName,
    ) -> path_std_io::Result<bool> {
        self.remove_from(extension_instance, provider, None)
    }

    /// Removes one profile from an inferred or explicitly selected source.
    pub(crate) fn remove_from(
        &self,
        extension_instance: &tau_proto::ExtensionName,
        provider: &tau_proto::ProviderName,
        requested_source: Option<ProfileSource>,
    ) -> path_std_io::Result<bool> {
        use fs2::FileExt as _;

        let settings_root = tau_config::settings::extension_provider_settings_dir_of(
            &self.state_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        ensure_private_directory_tree(
            &self.state_dir,
            &PathBuf::from("providers").join(extension_instance.as_str()),
        )?;
        let settings_lock = self
            .acquire_instance_lock(extension_instance)?
            .ok_or_else(|| {
                path_std_io::Error::new(
                    path_std_io::ErrorKind::NotFound,
                    "provider settings directory disappeared before locking",
                )
            })?;
        let settings_rel = PathBuf::from(format!("{provider}.json"));
        let config_root = tau_config::settings::extension_provider_config_dir_of(
            &self.config_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        let config_path = config_root.join(&settings_rel);
        reject_existing_symlink_components(&settings_root, &settings_rel)?;
        let state_path = settings_root.join(&settings_rel);
        let config_exists = config_path.exists();
        let state_exists = state_path.exists();
        let source = match (requested_source, config_exists, state_exists) {
            (_, true, true) => {
                return Err(path_std_io::Error::new(
                    path_std_io::ErrorKind::AlreadyExists,
                    "provider profile is duplicated across config and state",
                ));
            }
            (Some(ProfileSource::Config), true, false) | (None, true, false) => {
                ProfileSource::Config
            }
            (Some(ProfileSource::State), false, true) | (None, false, true) => ProfileSource::State,
            (Some(source), _, _) => {
                return Err(path_std_io::Error::new(
                    path_std_io::ErrorKind::NotFound,
                    format!("provider profile does not exist in {}", source.label()),
                ));
            }
            (None, false, false) => return Ok(false),
        };
        let settings_path = match source {
            ProfileSource::Config => config_path,
            ProfileSource::State => state_path,
        };
        let contents = read_provider_profile(
            &settings_path,
            match source {
                ProfileSource::Config => ProviderProfileLeafSymlinkPolicy::Follow,
                ProfileSource::State => ProviderProfileLeafSymlinkPolicy::Reject,
            },
        )?;
        let credential = credential_from_settings(&SetupProfile {
            provider: provider.clone(),
            source,
            path: settings_path.clone(),
            contents,
        })?;
        let removed = remove_file_sync(&settings_path)?;
        let secret_root = tau_config::settings::extension_secret_dir_of(
            &self.state_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        let secret_lock = match open_directory_no_follow(&secret_root) {
            Ok(lock) => {
                lock.lock_exclusive()?;
                Some(lock)
            }
            Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if let ProviderCredential::Stored(reference) = credential {
            let secret_rel = PathBuf::from("providers").join(reference.identity().as_str());
            reject_existing_symlink_components(&secret_root, &secret_rel)?;
            let _ = remove_file_sync(&secret_root.join(reference.path().as_str()))?;
        }
        if let Some(secret_lock) = secret_lock {
            fs2::FileExt::unlock(&secret_lock)?;
        }
        drop(settings_lock);
        Ok(removed)
    }

    /// Renames one provider profile file without inspecting its contents or
    /// changing its stable credential storage.
    pub(crate) fn rename_profile(
        &self,
        extension_instance: &tau_proto::ExtensionName,
        old: &tau_proto::ProviderName,
        new: &tau_proto::ProviderName,
    ) -> path_std_io::Result<ProfileSource> {
        if old == new {
            return Err(path_std_io::Error::new(
                path_std_io::ErrorKind::InvalidInput,
                "provider rename requires distinct names",
            ));
        }
        let state_root = tau_config::settings::extension_provider_settings_dir_of(
            &self.state_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        ensure_private_directory_tree(
            &self.state_dir,
            &PathBuf::from("providers").join(extension_instance.as_str()),
        )?;
        let _settings_lock = self
            .acquire_instance_lock(extension_instance)?
            .ok_or_else(|| {
                path_std_io::Error::new(
                    path_std_io::ErrorKind::NotFound,
                    "provider settings directory disappeared before locking",
                )
            })?;
        let config_root = tau_config::settings::extension_provider_config_dir_of(
            &self.config_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        let old_relative = PathBuf::from(format!("{old}.json"));
        let new_relative = PathBuf::from(format!("{new}.json"));
        reject_existing_symlink_components(&state_root, &old_relative)?;
        reject_existing_symlink_components(&state_root, &new_relative)?;
        let config_old = config_root.join(&old_relative);
        let state_old = state_root.join(&old_relative);
        let config_new = config_root.join(&new_relative);
        let state_new = state_root.join(&new_relative);
        let config_old_exists = path_exists(&config_old)?;
        let state_old_exists = path_exists(&state_old)?;
        let source = match (config_old_exists, state_old_exists) {
            (true, true) => {
                return Err(path_std_io::Error::new(
                    path_std_io::ErrorKind::AlreadyExists,
                    "provider profile is duplicated across config and state",
                ));
            }
            (true, false) => ProfileSource::Config,
            (false, true) => ProfileSource::State,
            (false, false) => {
                return Err(path_std_io::Error::new(
                    path_std_io::ErrorKind::NotFound,
                    "provider profile is not configured",
                ));
            }
        };
        if path_exists(&config_new)? || path_exists(&state_new)? {
            return Err(path_std_io::Error::new(
                path_std_io::ErrorKind::AlreadyExists,
                "new provider profile name already exists in config or state",
            ));
        }
        let (old_path, new_path) = match source {
            ProfileSource::Config => (config_old, config_new),
            ProfileSource::State => (state_old, state_new),
        };
        path_std_fs::rename(old_path, new_path)?;
        Ok(source)
    }

    /// Returns the selected instance's credential-free settings files.
    fn settings_files_unlocked(
        &self,
        extension_instance: &tau_proto::ExtensionName,
    ) -> path_std_io::Result<Vec<SetupProfile>> {
        let state_root = tau_config::settings::extension_provider_settings_dir_of(
            &self.state_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        reject_existing_symlink_components(&state_root, Path::new(""))?;
        let config_root = tau_config::settings::extension_provider_config_dir_of(
            &self.config_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        let mut files = Vec::new();
        let mut total_bytes = 0_u64;
        for (source, root) in [
            (ProfileSource::Config, config_root),
            (ProfileSource::State, state_root),
        ] {
            let entries = match path_std_fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let remaining = self.max_profile_files.saturating_sub(files.len());
            let mut entries = entries.take(remaining + 1).collect::<Result<Vec<_>, _>>()?;
            if remaining < entries.len() {
                return Err(path_std_io::Error::new(
                    path_std_io::ErrorKind::InvalidData,
                    format!(
                        "{} profile discovery exceeds {} files",
                        source.label(),
                        self.max_profile_files
                    ),
                ));
            }
            entries.sort_by_key(path_std_fs::DirEntry::file_name);
            let resolved_root = if source == ProfileSource::Config {
                Some(root.canonicalize()?)
            } else {
                None
            };
            for entry in entries {
                let presented_path = entry.path();
                match &resolved_root {
                    Some(_resolved_root) => {
                        let resolved = presented_path.canonicalize()?;
                        if !resolved.is_file() {
                            return Err(path_std_io::Error::new(
                                path_std_io::ErrorKind::InvalidInput,
                                "config profile does not resolve to a regular file",
                            ));
                        }
                    }
                    None if entry.file_type()?.is_file() => {}
                    None => {
                        return Err(path_std_io::Error::new(
                            path_std_io::ErrorKind::InvalidInput,
                            "state profile directory contains a non-regular entry",
                        ));
                    }
                }
                let name = presented_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        path_std_io::Error::new(
                            path_std_io::ErrorKind::InvalidInput,
                            "provider profile name is not UTF-8",
                        )
                    })?;
                let stem = name.strip_suffix(".json").ok_or_else(|| {
                    path_std_io::Error::new(
                        path_std_io::ErrorKind::InvalidInput,
                        "provider profile name must end in .json",
                    )
                })?;
                let provider = tau_proto::ProviderName::try_new(stem.to_owned()).map_err(|_| {
                    path_std_io::Error::new(
                        path_std_io::ErrorKind::InvalidInput,
                        "provider profile name is invalid",
                    )
                })?;
                let leaf_symlink_policy = match source {
                    ProfileSource::Config => ProviderProfileLeafSymlinkPolicy::Follow,
                    ProfileSource::State => ProviderProfileLeafSymlinkPolicy::Reject,
                };
                let contents = read_provider_profile(&presented_path, leaf_symlink_policy)
                    .map_err(|error| {
                        path_std_io::Error::new(
                            error.kind(),
                            format!("could not read {} profile: {error}", source.label()),
                        )
                    })?;
                total_bytes = total_bytes.saturating_add(contents.len() as u64);
                if MAX_PROVIDER_PROFILE_SNAPSHOT_BYTES < total_bytes {
                    return Err(path_std_io::Error::new(
                        path_std_io::ErrorKind::InvalidData,
                        format!(
                            "merged provider profile discovery exceeds \
                             {MAX_PROVIDER_PROFILE_SNAPSHOT_BYTES} bytes"
                        ),
                    ));
                }
                if files
                    .iter()
                    .any(|existing: &SetupProfile| existing.provider == provider)
                {
                    return Err(path_std_io::Error::new(
                        path_std_io::ErrorKind::AlreadyExists,
                        format!("provider '{provider}' is duplicated across config and state"),
                    ));
                }
                files.push(SetupProfile {
                    provider,
                    source,
                    path: presented_path,
                    contents,
                });
            }
        }
        files.sort_by(|left, right| left.provider.as_str().cmp(right.provider.as_str()));
        Ok(files)
    }

    /// Capture settings and matching credential slots under the universal
    /// instance-before-Secret lock order, releasing both before presentation.
    pub(crate) fn snapshot(
        &self,
        extension_instance: &tau_proto::ExtensionName,
    ) -> path_std_io::Result<SetupSnapshot> {
        use fs2::FileExt as _;

        let settings_lock = self.acquire_instance_lock(extension_instance)?;
        let _settings_lock = settings_lock;
        let settings = self.settings_files_unlocked(extension_instance)?;
        let secret_root = tau_config::settings::extension_secret_dir_of(
            &self.state_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        let secret_lock = match open_directory_no_follow(&secret_root) {
            Ok(lock) => {
                lock.lock_exclusive()?;
                Some(lock)
            }
            Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let mut credentials = BTreeMap::new();
        for profile in &settings {
            let Ok(ProviderCredential::Stored(reference)) = credential_from_settings(profile)
            else {
                continue;
            };
            let identity = reference.identity().clone();
            for slot in ProviderCredentialSlot::all() {
                match self.credential(extension_instance, &identity, slot) {
                    Ok(bytes) => {
                        credentials.insert((identity.clone(), slot), bytes);
                    }
                    Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
        }
        if let Some(secret_lock) = secret_lock {
            fs2::FileExt::unlock(&secret_lock)?;
        }
        Ok(SetupSnapshot {
            profiles: settings,
            credentials,
        })
    }

    /// Reads one deterministic credential slot for safe setup-status
    /// inspection.
    pub(crate) fn credential(
        &self,
        extension_instance: &tau_proto::ExtensionName,
        identity: &ProviderCredentialIdentity,
        slot: ProviderCredentialSlot,
    ) -> path_std_io::Result<Vec<u8>> {
        let root = tau_config::settings::extension_secret_dir_of(
            &self.state_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        let relative = PathBuf::from(slot.path(identity).as_str());
        reject_existing_symlink_components(&root, &relative)?;
        path_std_fs::read(root.join(relative))
    }
}

/// Parses the closed credential selection needed to locate one profile's Secret
/// records.
fn credential_from_settings(profile: &SetupProfile) -> path_std_io::Result<ProviderCredential> {
    let settings: serde_json::Value = serde_json::from_slice(&profile.contents).map_err(|_| {
        path_std_io::Error::new(
            path_std_io::ErrorKind::InvalidData,
            "provider profile is not valid JSON",
        )
    })?;
    let settings = settings.as_object().ok_or_else(|| {
        path_std_io::Error::new(
            path_std_io::ErrorKind::InvalidData,
            "provider profile is not a JSON object",
        )
    })?;
    parse_provider_credential(&profile.provider, settings)
        .map_err(|error| path_std_io::Error::new(path_std_io::ErrorKind::InvalidData, error))
}

/// Returns whether a directory entry exists, including a dangling symlink.
fn path_exists(path: &Path) -> path_std_io::Result<bool> {
    match path_std_fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn atomic_write(path: &Path, contents: &[u8]) -> path_std_io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        path_std_io::Error::new(
            path_std_io::ErrorKind::InvalidInput,
            "profile path has no parent",
        )
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write as _;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(unix)]
fn open_directory_no_follow(path: &Path) -> path_std_io::Result<path_std_fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    path_std_fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_directory_no_follow(path: &Path) -> path_std_io::Result<path_std_fs::File> {
    path_std_fs::File::open(path)
}

#[cfg(test)]
mod tests;

fn sanitize_secret_path(path: &str) -> path_std_io::Result<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(path_std_io::Error::new(
            path_std_io::ErrorKind::InvalidInput,
            "invalid provider secret path",
        ));
    }
    Ok(path.to_path_buf())
}

fn reject_existing_symlink_components(root: &Path, relative: &Path) -> path_std_io::Result<()> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        match path_std_fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(path_std_io::Error::other(
                    "provider storage path crosses a symlink",
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(path_std_io::Error::other(
                    "provider storage ancestor is not a directory",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        current.push(component.as_os_str());
    }
    if path_std_fs::symlink_metadata(current)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(path_std_io::Error::other(
            "provider storage destination is a symlink",
        ));
    }
    Ok(())
}

fn ensure_private_directory_tree(root: &Path, relative: &Path) -> path_std_io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    path_std_fs::create_dir_all(root)?;
    path_std_fs::set_permissions(root, path_std_fs::Permissions::from_mode(0o700))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match path_std_fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == path_std_io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = path_std_fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(path_std_io::Error::other(
                "provider storage path crosses a non-directory",
            ));
        }
        path_std_fs::set_permissions(&current, path_std_fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn atomic_private_write(path: &Path, contents: &[u8]) -> path_std_io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    use path_std_io::Write as _;

    let parent = path
        .parent()
        .ok_or_else(|| path_std_io::Error::other("provider path has no parent"))?;
    path_std_fs::create_dir_all(parent)?;
    path_std_fs::set_permissions(parent, path_std_fs::Permissions::from_mode(0o700))?;
    let temp = tempfile::NamedTempFile::with_prefix_in(".tau-provider-", parent)?;
    temp.as_file()
        .set_permissions(path_std_fs::Permissions::from_mode(0o600))?;
    let mut file = temp.reopen()?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    temp.persist(path).map_err(|error| error.error)?;
    path_std_fs::set_permissions(path, path_std_fs::Permissions::from_mode(0o600))?;
    path_std_fs::File::open(parent)?.sync_all()
}

fn remove_file_sync(path: &Path) -> path_std_io::Result<bool> {
    match path_std_fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                path_std_fs::File::open(parent)?.sync_all()?;
            }
            Ok(true)
        }
        Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}
