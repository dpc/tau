//! CLI-owned provider registration storage.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::{fs as path_std_fs, io as path_std_io};

use tau_config::provider_settings::{
    ProviderCredentialSlot, ProviderSettingsInstanceLock, ProviderSettingsLockAttempt,
};
use tau_config::secret_sources::{
    EnvironmentDisposition, load_secret_sources, resolve_declared_secret,
};

use crate::credential_record::ApiKeyCredential;

/// One complete provider registration publication.
pub(crate) struct ProviderSetupPlan {
    /// Stable configured provider-extension instance name.
    pub(crate) extension_instance: tau_proto::ExtensionName,
    /// Provider namespace used by settings and secret paths.
    pub(crate) provider: tau_proto::ProviderName,
    /// Credential-free provider settings JSON.
    pub(crate) settings: Vec<u8>,
    /// Complete typed credential record.
    pub(crate) secret: SecretWrite,
    /// Configured named source resolved inside the instance transaction.
    pub(crate) named_source: Option<NamedSecretSource>,
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
    /// Credential-free provider settings in deterministic provider order.
    pub(crate) settings: Vec<(tau_proto::ProviderName, Vec<u8>)>,
    /// Existing closed credential slots keyed by provider and family.
    pub(crate) credentials: BTreeMap<(tau_proto::ProviderName, ProviderCredentialSlot), Vec<u8>>,
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
    /// Tau user-state root.
    state_dir: PathBuf,
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
        let state_dir = tau_config::settings::state_dir().ok_or_else(|| {
            path_std_io::Error::new(
                path_std_io::ErrorKind::NotFound,
                "cannot determine Tau state directory",
            )
        })?;
        Ok(Self {
            state_dir,
            #[cfg(test)]
            contention: None,
            #[cfg(test)]
            acquired: None,
        })
    }

    #[cfg(test)]
    fn open_in(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            contention: None,
            acquired: None,
        }
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
    pub(crate) fn apply(&self, plan: &ProviderSetupPlan) -> path_std_io::Result<PathBuf> {
        let settings_root = tau_config::settings::extension_provider_settings_dir_of(
            &self.state_dir,
            plan.extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        ensure_private_directory_tree(
            &self.state_dir,
            &PathBuf::from("provider-settings").join(plan.extension_instance.as_str()),
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
        let named_contents = match &plan.named_source {
            Some(source) => {
                let sources = load_secret_sources(EnvironmentDisposition::Retain)
                    .map_err(path_std_io::Error::other)?;
                let value = resolve_declared_secret(
                    &self.state_dir,
                    &sources,
                    plan.extension_instance.as_str(),
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
                Some(
                    serde_json::to_vec(&ApiKeyCredential::new(value.expose_secret().to_owned()))
                        .map_err(path_std_io::Error::other)?,
                )
            }
            None => None,
        };
        let secret_root = tau_config::settings::extension_secret_dir_of(
            &self.state_dir,
            plan.extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        ensure_private_directory_tree(
            &self.state_dir,
            &PathBuf::from("secrets/ext").join(plan.extension_instance.as_str()),
        )?;
        use fs2::FileExt as _;
        let secret_lock = open_directory_no_follow(&secret_root)?;
        secret_lock.lock_exclusive()?;
        let secret_rel = sanitize_secret_path(plan.secret.path.as_str())?;
        reject_existing_symlink_components(&secret_root, &secret_rel)?;
        if let Some(parent) = secret_rel.parent() {
            ensure_private_directory_tree(&secret_root, parent)?;
        }
        atomic_private_write(
            &secret_root.join(secret_rel),
            named_contents
                .as_deref()
                .unwrap_or_else(|| plan.secret.contents.expose()),
        )?;
        fs2::FileExt::unlock(&secret_lock)?;
        let settings_rel = PathBuf::from(format!("{}.json", plan.provider));
        reject_existing_symlink_components(&settings_root, &settings_rel)?;
        let settings_path = settings_root.join(settings_rel);
        atomic_private_write(&settings_path, &plan.settings)?;
        Ok(settings_path)
    }

    /// Removes settings first and then every credential slot used by
    /// version-zero built-in providers.
    pub(crate) fn remove(
        &self,
        extension_instance: &tau_proto::ExtensionName,
        provider: &tau_proto::ProviderName,
    ) -> path_std_io::Result<bool> {
        use fs2::FileExt as _;

        let settings_root = tau_config::settings::extension_provider_settings_dir_of(
            &self.state_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        ensure_private_directory_tree(
            &self.state_dir,
            &PathBuf::from("provider-settings").join(extension_instance.as_str()),
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
        reject_existing_symlink_components(&settings_root, &settings_rel)?;
        let settings_path = settings_root.join(settings_rel);
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
        let secret_rel = PathBuf::from("providers").join(provider.as_str());
        reject_existing_symlink_components(&secret_root, &secret_rel)?;
        for slot in ProviderCredentialSlot::all() {
            let _ = remove_file_sync(&secret_root.join(slot.path(provider).as_str()))?;
        }
        if let Some(secret_lock) = secret_lock {
            fs2::FileExt::unlock(&secret_lock)?;
        }
        drop(settings_lock);
        Ok(removed)
    }

    /// Returns the selected instance's credential-free settings files.
    fn settings_files_unlocked(
        &self,
        extension_instance: &tau_proto::ExtensionName,
    ) -> path_std_io::Result<Vec<(tau_proto::ProviderName, Vec<u8>)>> {
        let root = tau_config::settings::extension_provider_settings_dir_of(
            &self.state_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        reject_existing_symlink_components(&root, Path::new(""))?;
        let entries = match path_std_fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(provider) = tau_proto::ProviderName::try_new(stem.to_owned()) else {
                continue;
            };
            let contents = path_std_fs::read(path)?;
            files.push((provider, contents));
        }
        files.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
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
        let Some(_settings_lock) = settings_lock else {
            return Ok(SetupSnapshot {
                settings: Vec::new(),
                credentials: BTreeMap::new(),
            });
        };
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
        for (provider, _) in &settings {
            for slot in ProviderCredentialSlot::all() {
                match self.credential(extension_instance, provider, slot) {
                    Ok(bytes) => {
                        credentials.insert((provider.clone(), slot), bytes);
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
            settings,
            credentials,
        })
    }

    /// Reads one deterministic credential slot for safe setup-status
    /// inspection.
    pub(crate) fn credential(
        &self,
        extension_instance: &tau_proto::ExtensionName,
        provider: &tau_proto::ProviderName,
        slot: ProviderCredentialSlot,
    ) -> path_std_io::Result<Vec<u8>> {
        let root = tau_config::settings::extension_secret_dir_of(
            &self.state_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        let relative = PathBuf::from(slot.path(provider).as_str());
        reject_existing_symlink_components(&root, &relative)?;
        path_std_fs::read(root.join(relative))
    }
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
