//! Filesystem durability helpers for startup retention staging.

#[cfg(test)]
mod tests;

use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;

/// Opens and synchronizes one exact real directory without following its final
/// path component.
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let directory = options.open(path)?;
    if !directory.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "retention sync target is not a directory",
        ));
    }
    directory.sync_all()
}

/// Creates or validates a staging directory and durably publishes its own name.
pub(crate) fn prepare_staging_directory(
    staging_dir: &Path,
    sync: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    match fs::symlink_metadata(staging_dir) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "retention staging root is not a real directory",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(staging_dir)?,
        Err(error) => return Err(error),
    }
    let parent = staging_dir.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "retention staging root has no parent",
        )
    })?;
    sync(parent)?;
    sync(staging_dir)
}

/// Commits a cross-directory detach before recursive removal may begin.
///
/// Synchronizing the staging parent first prevents durable source removal from
/// outrunning publication of the restart-finalizable staging name.
pub(crate) fn sync_detach_boundary(
    source_parent: &Path,
    staging_parent: &Path,
    sync: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    sync(staging_parent)?;
    sync(source_parent)
}

/// Recursively removes one staged tree and durably records its disappearance.
pub(crate) fn remove_staged_tree(
    path: &Path,
    staging_parent: &Path,
    remove: &mut dyn FnMut(&Path) -> io::Result<()>,
    sync: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    remove(path)?;
    sync(staging_parent)
}
