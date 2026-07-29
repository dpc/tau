//! Atomic file write helper that follows symlinks at the destination.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Atomically write `contents` to `path` by creating a randomized sibling
/// temporary file and renaming it over the destination.
///
/// If `path` is a symlink, the symlink's target is replaced rather than the
/// symlink itself — preserving user-managed indirection (e.g. dotfile
/// managers).
///
/// Existing-file permissions are preserved across the replace. If the
/// destination does not exist, `default_permissions` is applied to the new
/// file (use this to enforce e.g. `0o600` for credential files); pass `None`
/// to let the umask decide.
pub fn atomic_write_following_symlink(
    path: &Path,
    contents: &[u8],
    default_permissions: Option<Permissions>,
) -> io::Result<()> {
    let destination = symlink_target_or_path(path)?;

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let permissions = fs::metadata(&destination)
        .ok()
        .map(|metadata| metadata.permissions())
        .or(default_permissions);

    let mut temp_path = destination.clone();
    let mut temp_file = loop {
        let suffix: u64 = rand::random();
        let file_name = destination
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        temp_path.set_file_name(format!(".{file_name}.{suffix:016x}.tmp"));

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(permissions) = &permissions {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

            options.mode(permissions.mode() & 0o777);
        }

        match options.open(&temp_path) {
            Ok(mut file) => {
                let temp_file = PendingTempFile::new(temp_path.clone());
                if let Some(permissions) = permissions.clone() {
                    file.set_permissions(permissions)?;
                }
                file.write_all(contents)?;
                file.sync_all()?;
                break temp_file;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };

    fs::rename(&temp_file.path, &destination)?;
    temp_file.disarm();

    sync_parent_dir(&destination)?;

    Ok(())
}

struct PendingTempFile {
    path: PathBuf,
    armed: bool,
}

impl PendingTempFile {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingTempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn sync_parent_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn symlink_target_or_path(path: &Path) -> io::Result<PathBuf> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(path.to_path_buf()),
        Err(error) => return Err(error),
    };

    if !metadata.file_type().is_symlink() {
        return Ok(path.to_path_buf());
    }

    let target = fs::read_link(path)?;
    if target.is_absolute() {
        Ok(target)
    } else if let Some(parent) = path.parent() {
        Ok(parent.join(target))
    } else {
        Ok(target)
    }
}

#[cfg(test)]
mod tests;
