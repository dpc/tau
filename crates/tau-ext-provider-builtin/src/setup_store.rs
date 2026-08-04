//! CLI-owned provider registration storage.

use std::path::{Component, Path, PathBuf};
use std::{fs as path_std_fs, io as path_std_io};

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

/// Closed credential slots owned by version-zero built-in providers.
#[derive(Clone, Copy)]
pub(crate) enum CredentialSlot {
    /// ChatGPT OAuth record.
    OAuth,
    /// API-key record.
    ApiKey,
}

impl CredentialSlot {
    /// Returns the only filename owned by this credential family.
    fn file_name(self) -> &'static str {
        match self {
            Self::OAuth => "oauth.json",
            Self::ApiKey => "api-key.json",
        }
    }

    fn all() -> [Self; 2] {
        [Self::OAuth, Self::ApiKey]
    }

    /// Builds the exact Secret-scope path for one provider registration.
    pub(crate) fn path(self, provider: &tau_proto::ProviderName) -> tau_proto::ExtensionDataPath {
        tau_proto::ExtensionDataPath::new(format!("providers/{provider}/{}", self.file_name()))
    }

    /// Returns the credential-reference discriminator persisted in settings.
    pub(crate) fn reference_kind(self) -> &'static str {
        match self {
            Self::OAuth => "oauth",
            Self::ApiKey => "api_key",
        }
    }
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
        Ok(Self { state_dir })
    }

    #[cfg(test)]
    fn open_in(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
        }
    }

    /// Applies secret-first and settings-last, making the settings write the
    /// registration activation point.
    pub(crate) fn apply(&self, plan: &ProviderSetupPlan) -> path_std_io::Result<PathBuf> {
        use fs2::FileExt as _;

        let secret_root = tau_config::settings::extension_secret_dir_of(
            &self.state_dir,
            plan.extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        ensure_private_directory_tree(
            &self.state_dir,
            &PathBuf::from("secrets/ext").join(plan.extension_instance.as_str()),
        )?;
        let secret_lock = path_std_fs::File::open(&secret_root)?;
        secret_lock.lock_exclusive()?;
        let secret_rel = sanitize_secret_path(plan.secret.path.as_str())?;
        reject_existing_symlink_components(&secret_root, &secret_rel)?;
        if let Some(parent) = secret_rel.parent() {
            ensure_private_directory_tree(&secret_root, parent)?;
        }
        atomic_private_write(&secret_root.join(secret_rel), plan.secret.contents.expose())?;
        fs2::FileExt::unlock(&secret_lock)?;

        let settings_root = tau_config::settings::extension_provider_settings_dir_of(
            &self.state_dir,
            plan.extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        ensure_private_directory_tree(
            &self.state_dir,
            &PathBuf::from("provider-settings").join(plan.extension_instance.as_str()),
        )?;
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
        let settings_rel = PathBuf::from(format!("{provider}.json"));
        reject_existing_symlink_components(&settings_root, &settings_rel)?;
        let settings_path = settings_root.join(settings_rel);
        let removed = remove_file_sync(&settings_path)?;
        let secret_root = tau_config::settings::extension_secret_dir_of(
            &self.state_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        let secret_lock = match path_std_fs::File::open(&secret_root) {
            Ok(lock) => {
                lock.lock_exclusive()?;
                Some(lock)
            }
            Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let secret_rel = PathBuf::from("providers").join(provider.as_str());
        reject_existing_symlink_components(&secret_root, &secret_rel)?;
        for slot in CredentialSlot::all() {
            let _ = remove_file_sync(
                &secret_root
                    .join("providers")
                    .join(provider.as_str())
                    .join(slot.file_name()),
            )?;
        }
        if let Some(secret_lock) = secret_lock {
            fs2::FileExt::unlock(&secret_lock)?;
        }
        Ok(removed)
    }

    /// Returns the selected instance's credential-free settings files.
    pub(crate) fn settings_files(
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

    /// Reads one deterministic credential slot for safe setup-status
    /// inspection.
    pub(crate) fn credential(
        &self,
        extension_instance: &tau_proto::ExtensionName,
        provider: &tau_proto::ProviderName,
        slot: CredentialSlot,
    ) -> path_std_io::Result<Vec<u8>> {
        let root = tau_config::settings::extension_secret_dir_of(
            &self.state_dir,
            extension_instance.as_str(),
        )
        .map_err(path_std_io::Error::other)?;
        let relative = PathBuf::from("providers")
            .join(provider.as_str())
            .join(slot.file_name());
        reject_existing_symlink_components(&root, &relative)?;
        path_std_fs::read(root.join(relative))
    }
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
