//! Injectable filesystem operations for deterministic persistence fault tests.

use std::fs::{DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::Path;

use fs2::FileExt;

/// Every mutable filesystem operation used by the production worker.
///
/// Deterministic tests inject the same interface to fail or block exact state
/// transitions without changing paths or bypassing production scheduling.
pub(crate) trait PersistenceBackend: Send + Sync + 'static {
    /// Creates a private directory hierarchy during explicit preparation.
    fn create_owner_directories(&self, path: &Path) -> io::Result<()>;
    /// Creates exactly one directory and fails if it already exists.
    fn create_owner_directory(&self, path: &Path) -> io::Result<()>;
    /// Applies owner-private permissions to one filesystem object.
    fn set_permissions(&self, path: &Path, permissions: Permissions) -> io::Result<()>;
    /// Opens a new owner-private read/write file.
    fn create_new_file(&self, path: &Path) -> io::Result<File>;
    /// Opens an existing read/write file.
    fn open_existing_file(&self, path: &Path) -> io::Result<File>;
    /// Opens a truncate-or-create owner-private write file.
    fn create_temporary_file(&self, path: &Path) -> io::Result<File>;
    /// Acquires the exclusive advisory stream lock.
    fn try_lock(&self, file: &File) -> io::Result<()>;
    /// Captures and seeks to exact EOF.
    fn seek_end(&self, file: &mut File) -> io::Result<u64>;
    /// Writes all supplied bytes or an exact injected prefix.
    fn write_all(&self, file: &mut File, bytes: &[u8]) -> io::Result<()>;
    /// Restores exact EOF after a failed frame.
    fn truncate(&self, file: &File, offset: u64) -> io::Result<()>;
    /// Synchronizes complete journal data.
    fn sync_data(&self, file: &File) -> io::Result<()>;
    /// Captures the exact journal identity and boundary at a complete frame.
    fn journal_position(
        &self,
        file: &File,
        end_offset: u64,
    ) -> io::Result<crate::agent_checkpoint::CommittedJournalPosition>;
    /// Synchronizes a file or directory including metadata.
    fn sync_all(&self, file: &File) -> io::Result<()>;
    /// Atomically replaces one sibling path.
    fn rename(&self, source: &Path, destination: &Path) -> io::Result<()>;
    /// Removes a failed temporary file best-effort.
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Opens one directory for synchronization.
    fn open_directory(&self, path: &Path) -> io::Result<File>;
    /// Reads one bounded preparation input.
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;
}

/// Ordinary production filesystem implementation.
pub(crate) struct FilesystemBackend;

impl PersistenceBackend for FilesystemBackend {
    fn create_owner_directories(&self, path: &Path) -> io::Result<()> {
        let mut builder = DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            builder.mode(0o700);
        }
        builder.create(path)?;
        #[cfg(unix)]
        self.set_permissions(path, Permissions::from_mode(0o700))?;
        Ok(())
    }

    fn create_owner_directory(&self, path: &Path) -> io::Result<()> {
        let mut builder = DirBuilder::new();
        #[cfg(unix)]
        {
            builder.mode(0o700);
        }
        builder.create(path)
    }

    fn set_permissions(&self, path: &Path, permissions: Permissions) -> io::Result<()> {
        std::fs::set_permissions(path, permissions)
    }

    fn create_new_file(&self, path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.create_new(true).read(true).write(true);
        set_owner_file_mode(&mut options);
        options.open(path)
    }

    fn create_temporary_file(&self, path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        set_owner_file_mode(&mut options);
        let file = options.open(path)?;
        #[cfg(unix)]
        self.set_permissions(path, Permissions::from_mode(0o600))?;
        Ok(file)
    }

    fn open_existing_file(&self, path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).open(path)
    }

    fn try_lock(&self, file: &File) -> io::Result<()> {
        file.try_lock_exclusive()
    }

    fn seek_end(&self, file: &mut File) -> io::Result<u64> {
        file.seek(SeekFrom::End(0))
    }

    fn write_all(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes)
    }

    fn truncate(&self, file: &File, offset: u64) -> io::Result<()> {
        file.set_len(offset)
    }

    fn sync_data(&self, file: &File) -> io::Result<()> {
        file.sync_data()
    }

    fn journal_position(
        &self,
        file: &File,
        end_offset: u64,
    ) -> io::Result<crate::agent_checkpoint::CommittedJournalPosition> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileExt as _, MetadataExt as _};
            let metadata = file.metadata()?;
            let boundary_len = end_offset.min(64) as usize;
            let mut boundary = vec![0; boundary_len];
            let start = end_offset.saturating_sub(boundary_len as u64);
            file.read_exact_at(&mut boundary, start)?;
            Ok(crate::agent_checkpoint::CommittedJournalPosition {
                device: metadata.dev(),
                inode: metadata.ino(),
                end_offset,
                boundary,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (file, end_offset);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "managed checkpoints require file identity support",
            ))
        }
    }

    fn sync_all(&self, file: &File) -> io::Result<()> {
        file.sync_all()
    }

    fn rename(&self, source: &Path, destination: &Path) -> io::Result<()> {
        std::fs::rename(source, destination)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn open_directory(&self, path: &Path) -> io::Result<File> {
        File::open(path)
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

fn set_owner_file_mode(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
}
