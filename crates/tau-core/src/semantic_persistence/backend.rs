//! Injectable filesystem operations for deterministic persistence fault tests.

use std::fs::{DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::Path;

use fs2::FileExt;

/// Final-component shape observed at one existing persistence path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExistingPathKind {
    /// A real directory, not a final-component symbolic link.
    Directory,
    /// A symbolic link at the exact requested path.
    Symlink,
    /// Any other filesystem object.
    Other,
}

/// Every mutable filesystem operation used by the production worker.
///
/// Deterministic tests inject the same interface to fail or block exact state
/// transitions without changing paths or bypassing production scheduling.
pub(crate) trait PersistenceBackend: Send + Sync + 'static {
    /// Creates a private directory hierarchy during explicit preparation.
    fn create_owner_directories(&self, path: &Path) -> io::Result<()>;
    /// Creates exactly one directory and fails if it already exists.
    fn create_owner_directory(&self, path: &Path) -> io::Result<()>;
    /// Classifies one existing path without following its final component.
    fn existing_path_kind(&self, path: &Path) -> io::Result<ExistingPathKind>;
    /// Applies owner-private permissions to one filesystem object.
    fn set_permissions(&self, path: &Path, permissions: Permissions) -> io::Result<()>;
    /// Opens a new owner-private read/write file.
    fn create_new_file(&self, path: &Path) -> io::Result<File>;
    /// Opens an existing read/write file.
    fn open_existing_file(&self, path: &Path) -> io::Result<File>;
    /// Opens an existing regular read-only file without following its final
    /// symbolic-link component.
    fn open_existing_regular_file_read_no_follow(&self, path: &Path) -> io::Result<File>;
    /// Opens an existing regular read/write file without following its final
    /// symbolic-link component.
    fn open_existing_regular_file_write_no_follow(&self, path: &Path) -> io::Result<File>;
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
    /// Atomically publishes one sibling hard link only when the destination is
    /// absent, leaving source-name cleanup to the caller.
    fn publish_no_replace(&self, source: &Path, destination: &Path) -> io::Result<()>;
    /// Removes a failed temporary file best-effort.
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Opens one directory for synchronization.
    fn open_directory(&self, path: &Path) -> io::Result<File>;
    /// Reads one bounded preparation input.
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Reads one already-open preparation input from its beginning.
    fn read_open_file(&self, file: &File) -> io::Result<Vec<u8>>;
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

    fn existing_path_kind(&self, path: &Path) -> io::Result<ExistingPathKind> {
        let metadata = std::fs::symlink_metadata(path)?;
        Ok(if metadata.file_type().is_symlink() {
            ExistingPathKind::Symlink
        } else if metadata.is_dir() {
            ExistingPathKind::Directory
        } else {
            ExistingPathKind::Other
        })
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

    fn open_existing_regular_file_read_no_follow(&self, path: &Path) -> io::Result<File> {
        open_regular_file_no_follow(path, false)
    }

    fn open_existing_regular_file_write_no_follow(&self, path: &Path) -> io::Result<File> {
        open_regular_file_no_follow(path, true)
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
        let (device, inode) = crate::agent_checkpoint::file_identity(file)?;
        let boundary_len = end_offset.min(64) as usize;
        let mut boundary = vec![0; boundary_len];
        let start = end_offset.saturating_sub(boundary_len as u64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt as _;
            file.read_exact_at(&mut boundary, start)?;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt as _;
            let mut filled = 0;
            while filled < boundary.len() {
                let read = file.seek_read(
                    &mut boundary[filled..],
                    start + u64::try_from(filled).expect("boundary offset is bounded"),
                )?;
                if read == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "managed checkpoint boundary ended early",
                    ));
                }
                filled += read;
            }
        }
        #[cfg(not(any(unix, windows)))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "managed checkpoints require positional reads",
        ));
        Ok(crate::agent_checkpoint::CommittedJournalPosition {
            device,
            inode,
            end_offset,
            boundary,
        })
    }

    fn sync_all(&self, file: &File) -> io::Result<()> {
        file.sync_all()
    }

    fn rename(&self, source: &Path, destination: &Path) -> io::Result<()> {
        std::fs::rename(source, destination)
    }

    fn publish_no_replace(&self, source: &Path, destination: &Path) -> io::Result<()> {
        std::fs::hard_link(source, destination)
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

    fn read_open_file(&self, file: &File) -> io::Result<Vec<u8>> {
        let mut reader = file.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

fn open_regular_file_no_follow(path: &Path, writable: bool) -> io::Result<File> {
    let file = open_no_follow(path, writable)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "canonical session artifact is not a regular file: {}",
                path.display()
            ),
        ));
    }
    clear_nonblocking(&file)?;
    Ok(file)
}

#[cfg(unix)]
fn open_no_follow(path: &Path, writable: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(writable)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(windows)]
fn open_no_follow(path: &Path, writable: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(writable)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_no_follow(_path: &Path, _writable: bool) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "exact no-follow session admission is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn clear_nonblocking(file: &File) -> io::Result<()> {
    use rustix_v1::fs::{OFlags, fcntl_getfl, fcntl_setfl};

    let flags = fcntl_getfl(file)?;
    if !flags.contains(OFlags::NONBLOCK) {
        return Ok(());
    }
    fcntl_setfl(file, flags - OFlags::NONBLOCK)?;
    Ok(())
}

#[cfg(not(unix))]
fn clear_nonblocking(_file: &File) -> io::Result<()> {
    Ok(())
}

fn set_owner_file_mode(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
}
