//! Typed provider authentication storage.
//!
//! Built-in provider extensions own their auth schema. This module only owns
//! the common filesystem mechanics: locating `auth.d/`, serializing one JSON
//! file, taking a sidecar lock, and writing with atomic replacement.

use std::fs::{File, OpenOptions};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::{fs, io};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tau_config::atomic::atomic_write_following_symlink;

mod auth_file_lock_result;

pub use auth_file_lock_result::AuthFileLockResult;
use auth_file_lock_result::{classify_callback_result, into_legacy_result};

/// Returns the auth state directory.
///
/// Prefers `XDG_STATE_HOME/tau` (`~/.local/state/tau` on Linux). Falls back to
/// `data_local_dir/tau` on platforms where `state_dir` is not available.
fn state_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|d| d.join("tau"))
}

/// A filesystem-backed provider auth store.
#[derive(Clone, Debug)]
pub struct ProviderStore {
    state_dir: PathBuf,
}

impl ProviderStore {
    /// Open the default Tau provider auth store.
    pub fn open_default() -> io::Result<Self> {
        let state_dir = state_dir().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "cannot determine data directory")
        })?;
        Ok(Self { state_dir })
    }

    /// Open a provider auth store rooted at `state_dir`.
    ///
    /// This is intended for tests and tools that need to operate on an
    /// alternate Tau state directory.
    pub fn open_in(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
        }
    }

    /// Returns the per-provider auth directory `auth.d/`.
    pub fn auth_dir(&self) -> PathBuf {
        self.state_dir.join("auth.d")
    }

    /// Returns a typed handle for one auth JSON file under `auth.d/`.
    ///
    /// `name` is the file stem, without `.json`; built-in provider profiles
    /// use their provider namespace as this stable file stem.
    pub fn auth_file<T>(&self, name: impl Into<String>) -> io::Result<AuthFile<T>> {
        AuthFile::new(self.clone(), name)
    }
}

/// Typed handle for one provider extension auth file.
#[derive(Clone, Debug)]
pub struct AuthFile<T> {
    store: ProviderStore,
    name: String,
    _marker: PhantomData<T>,
}

impl<T> AuthFile<T> {
    /// Open `auth.d/<name>.json` in the default provider store.
    pub fn open_default(name: impl Into<String>) -> io::Result<Self> {
        ProviderStore::open_default()?.auth_file(name)
    }

    /// Create a typed auth-file handle in `store`.
    pub fn new(store: ProviderStore, name: impl Into<String>) -> io::Result<Self> {
        let name = name.into();
        validate_auth_file_name(&name)?;
        Ok(Self {
            store,
            name,
            _marker: PhantomData,
        })
    }

    /// Stable file stem for this auth file.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Path to `auth.d/<name>.json`.
    pub fn path(&self) -> PathBuf {
        self.store.auth_dir().join(format!("{}.json", self.name))
    }

    /// Path to the sidecar lock for this auth file.
    pub fn lock_path(&self) -> PathBuf {
        self.store.auth_dir().join(format!("{}.lock", self.name))
    }

    /// Run a callback while holding the exclusive sidecar lock for this auth
    /// file.
    pub fn with_lock<R>(
        &self,
        f: impl FnOnce(&LockedAuthFile<'_, T>) -> io::Result<R>,
    ) -> io::Result<R> {
        into_legacy_result(self.with_lock_result(f))
    }

    /// Run a callback under the exclusive sidecar lock while preserving the
    /// callback outcome separately from lock-release failure.
    pub fn with_lock_result<R>(
        &self,
        f: impl FnOnce(&LockedAuthFile<'_, T>) -> io::Result<R>,
    ) -> AuthFileLockResult<R> {
        let lock_file = match self.open_lock_file() {
            Ok(lock_file) => lock_file,
            Err(error) => return AuthFileLockResult::LockFailed(error),
        };
        if let Err(error) = lock_file.lock() {
            return AuthFileLockResult::LockFailed(error);
        }
        let locked = LockedAuthFile {
            auth_file: self,
            lock_file,
        };
        let result = f(&locked);
        classify_callback_result(result, locked.lock_file.unlock())
    }

    fn open_lock_file(&self) -> io::Result<File> {
        let path = self.lock_path();
        let dir = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no parent for provider lock path")
        })?;
        create_private_dir(dir)?;
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
    }
}

impl<T> AuthFile<T>
where
    T: Serialize + DeserializeOwned,
{
    /// Load this auth file without taking the sidecar lock.
    ///
    /// Missing files yield `None`; present files must deserialize as `T`.
    pub fn load(&self) -> io::Result<Option<T>> {
        match fs::read_to_string(self.path()) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Save this auth file while holding its sidecar lock.
    pub fn save(&self, value: &T) -> io::Result<()> {
        self.with_lock(|locked| locked.save(value))
    }
}

impl<T> AuthFile<T> {
    /// Delete this auth file while holding its sidecar lock.
    ///
    /// Returns true when an auth JSON file existed and was removed.
    pub fn delete(&self) -> io::Result<bool> {
        self.with_lock(|locked| locked.delete())
    }
}

/// A typed auth-file handle whose sidecar lock is held.
pub struct LockedAuthFile<'a, T> {
    auth_file: &'a AuthFile<T>,
    lock_file: File,
}

impl<T> LockedAuthFile<'_, T>
where
    T: Serialize + DeserializeOwned,
{
    /// Load this auth file while its sidecar lock is held.
    pub fn load(&self) -> io::Result<Option<T>> {
        self.auth_file.load()
    }

    /// Save this auth file while its sidecar lock is held.
    pub fn save(&self, value: &T) -> io::Result<()> {
        let path = self.auth_file.path();
        let dir = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no parent for provider auth path")
        })?;
        create_private_dir(dir)?;
        let json = serde_json::to_string_pretty(value)?;

        #[cfg(unix)]
        let default_permissions = {
            use std::os::unix::fs::PermissionsExt;
            Some(fs::Permissions::from_mode(0o600))
        };
        #[cfg(not(unix))]
        let default_permissions = None;

        atomic_write_following_symlink(&path, json.as_bytes(), default_permissions)
    }
}

impl<T> LockedAuthFile<'_, T> {
    /// Remove this auth file while its sidecar lock is held.
    pub fn delete(&self) -> io::Result<bool> {
        match fs::remove_file(self.auth_file.path()) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn validate_auth_file_name(name: &str) -> io::Result<()> {
    if name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "auth file name must be non-empty",
        ));
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("auth file name '{name}' may not start with '.' or '-'"),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "auth file name '{name}' may only contain ASCII letters, digits, '_', '-', '.'"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
