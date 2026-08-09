//! Per-instance provider settings lifecycle lock.

use std::path::{Path, PathBuf};
use std::{fmt, fs, io};

/// Exclusive lifecycle ownership for one configured provider instance.
///
/// Holding this lock serializes setup/removal against the startup snapshot and
/// named-credential publication transaction.
pub struct ProviderSettingsInstanceLock {
    /// Canonical providers directory used as both data root and lock.
    root: PathBuf,
    /// Open directory handle retaining the process-scoped exclusive lock.
    _directory: fs::File,
}

/// Outcome of a nonblocking attempt to own one provider instance lifecycle.
pub enum ProviderSettingsLockAttempt {
    /// The instance settings directory does not exist.
    Missing,
    /// The caller now owns the instance lifecycle lock.
    Acquired(ProviderSettingsInstanceLock),
    /// Another caller currently owns the instance lifecycle lock.
    Contended,
}

impl fmt::Debug for ProviderSettingsInstanceLock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderSettingsInstanceLock")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl ProviderSettingsInstanceLock {
    /// Ensures and locks the Tau-private mutable lifecycle directory for a
    /// persistent provider instance.
    pub fn acquire_or_create(state_dir: &Path, extension_instance: &str) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;

        crate::settings::validate_extension_name(extension_instance).map_err(io::Error::other)?;
        for directory in [
            state_dir.join("providers"),
            state_dir.join("providers").join(extension_instance),
        ] {
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            let metadata = fs::symlink_metadata(&directory)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "provider lifecycle lock path is not a real directory",
                ));
            }
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        Self::acquire_existing(state_dir, extension_instance)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "provider lifecycle directory disappeared before locking",
            )
        })
    }

    /// Lock an existing providers directory without following its leaf
    /// if it is a symlink. A missing directory means the instance has no
    /// registrations and returns `None`.
    pub fn acquire_existing(
        state_dir: &Path,
        extension_instance: &str,
    ) -> io::Result<Option<Self>> {
        use fs2::FileExt as _;

        let Some((root, directory)) = open_existing(state_dir, extension_instance)? else {
            return Ok(None);
        };
        directory.lock_exclusive()?;
        Ok(Some(Self {
            root,
            _directory: directory,
        }))
    }

    /// Try to lock an existing instance without waiting for its current owner.
    pub fn try_acquire_existing(
        state_dir: &Path,
        extension_instance: &str,
    ) -> io::Result<ProviderSettingsLockAttempt> {
        use fs2::FileExt as _;

        let Some((root, directory)) = open_existing(state_dir, extension_instance)? else {
            return Ok(ProviderSettingsLockAttempt::Missing);
        };
        match directory.try_lock_exclusive() {
            Ok(()) => Ok(ProviderSettingsLockAttempt::Acquired(Self {
                root,
                _directory: directory,
            })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                Ok(ProviderSettingsLockAttempt::Contended)
            }
            Err(error) => Err(error),
        }
    }

    /// Return the locked providers directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn open_existing(
    state_dir: &Path,
    extension_instance: &str,
) -> io::Result<Option<(PathBuf, fs::File)>> {
    let root = crate::settings::extension_provider_settings_dir_of(state_dir, extension_instance)
        .map_err(io::Error::other)?;
    let provider_settings_root = state_dir.join("providers");
    for directory in [&provider_settings_root, &root] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "provider settings lock path is not a real directory",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
    }
    let directory = match open_directory_no_follow(&root) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some((root, directory)))
}

#[cfg(unix)]
fn open_directory_no_follow(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_directory_no_follow(path: &Path) -> io::Result<fs::File> {
    let file = fs::File::open(path)?;
    if file.metadata()?.is_dir() {
        Ok(file)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "provider settings root is not a directory",
        ))
    }
}
