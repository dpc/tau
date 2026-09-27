//! Extension-owned persistent data filesystem helpers.
//!
//! This module keeps the path validation, symlink rejection, and atomic file
//! update rules for `ExtensionDataRequest` outside of the central harness
//! event loop.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::{fs as path_std_fs, io as path_std_io, path as path_std_path, time as path_std_time};

pub(super) use tau_config::secret_sources::MAX_SECRET_DATA_FILE_BYTES;

use crate::EXTENSION_DATA_MAX_FILE_BYTES;
/// Maximum directory entries scanned by one extension data list operation.
const MAX_EXTENSION_DATA_LIST_ENTRIES: usize = 4096;

/// Error returned while serving an extension data operation.
#[derive(Debug)]
pub(super) struct ExtensionDataError {
    /// Protocol error category reported to the requesting extension.
    pub(super) kind: tau_proto::ExtensionDataErrorKind,
    /// Human-readable error message reported to the requesting extension.
    pub(super) message: String,
}

impl ExtensionDataError {
    /// Builds an extension data error from a protocol kind and message.
    pub(super) fn new(kind: tau_proto::ExtensionDataErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn io(message: impl Into<String>, error: std::io::Error) -> Self {
        let kind = match error.kind() {
            path_std_io::ErrorKind::NotFound => tau_proto::ExtensionDataErrorKind::NotFound,
            path_std_io::ErrorKind::AlreadyExists => {
                tau_proto::ExtensionDataErrorKind::AlreadyExists
            }
            path_std_io::ErrorKind::PermissionDenied => {
                tau_proto::ExtensionDataErrorKind::Permission
            }
            _ => tau_proto::ExtensionDataErrorKind::Io,
        };
        Self::new(kind, format!("{}: {error}", message.into()))
    }
}

pub(super) fn sanitize_extension_data_path(
    path: &str,
    allow_empty: bool,
) -> Result<PathBuf, ExtensionDataError> {
    if path.is_empty() {
        return if allow_empty {
            Ok(PathBuf::new())
        } else {
            Err(ExtensionDataError::new(
                tau_proto::ExtensionDataErrorKind::InvalidPath,
                "path must not be empty",
            ))
        };
    }
    let input = Path::new(path);
    if input.is_absolute() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::InvalidPath,
            "path must be relative",
        ));
    }
    let mut out = PathBuf::new();
    for component in input.components() {
        match component {
            path_std_path::Component::Normal(part) => out.push(part),
            path_std_path::Component::CurDir => {
                return Err(ExtensionDataError::new(
                    tau_proto::ExtensionDataErrorKind::InvalidPath,
                    "path must not contain `.`",
                ));
            }
            path_std_path::Component::ParentDir => {
                return Err(ExtensionDataError::new(
                    tau_proto::ExtensionDataErrorKind::InvalidPath,
                    "path must not contain `..`",
                ));
            }
            path_std_path::Component::RootDir | path_std_path::Component::Prefix(_) => {
                return Err(ExtensionDataError::new(
                    tau_proto::ExtensionDataErrorKind::InvalidPath,
                    "path must be relative",
                ));
            }
        }
    }
    if out.as_os_str().is_empty() && !allow_empty {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::InvalidPath,
            "path must not be empty",
        ));
    }
    Ok(out)
}

pub(super) fn checked_extension_data_path(
    root: &Path,
    rel: &Path,
    allow_missing_leaf: bool,
) -> Result<PathBuf, ExtensionDataError> {
    std::fs::create_dir_all(root)
        .map_err(|error| ExtensionDataError::io("failed to create extension data root", error))?;
    let root_metadata = std::fs::symlink_metadata(root)
        .map_err(|error| ExtensionDataError::io("failed to stat extension data root", error))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::NotDir,
            "extension data root is not a real directory",
        ));
    }
    set_private_dir_permissions(root)
        .map_err(|error| ExtensionDataError::io("failed to chmod extension data root", error))?;
    reject_symlink_ancestors(root, rel)?;
    if allow_missing_leaf {
        create_private_ancestor_dirs(root, rel)?;
    }
    let full = root.join(rel);
    match std::fs::symlink_metadata(&full) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::InvalidPath,
            format!("path `{}` is a symlink", rel.display()),
        )),
        Ok(metadata) if metadata.is_dir() || metadata.is_file() => Ok(full),
        Ok(_) => Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::NotFile,
            format!("path `{}` is not a file or directory", rel.display()),
        )),
        Err(error) if allow_missing_leaf && error.kind() == path_std_io::ErrorKind::NotFound => {
            Ok(full)
        }
        Err(error) => Err(ExtensionDataError::io(
            format!("failed to stat `{}`", rel.display()),
            error,
        )),
    }
}

fn create_private_ancestor_dirs(root: &Path, rel: &Path) -> Result<(), ExtensionDataError> {
    let mut current = root.to_path_buf();
    let mut components = rel.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        current.push(component.as_os_str());
        match std::fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == path_std_io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(ExtensionDataError::io(
                    "failed to create extension data directory",
                    error,
                ));
            }
        }
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|error| ExtensionDataError::io("failed to inspect data directory", error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ExtensionDataError::new(
                tau_proto::ExtensionDataErrorKind::NotDir,
                "extension data path ancestor is not a real directory",
            ));
        }
        set_private_dir_permissions(&current)
            .map_err(|error| ExtensionDataError::io("failed to chmod data directory", error))?;
    }
    Ok(())
}

fn reject_symlink_ancestors(root: &Path, rel: &Path) -> Result<(), ExtensionDataError> {
    let mut current = root.to_path_buf();
    let mut components = rel.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ExtensionDataError::new(
                    tau_proto::ExtensionDataErrorKind::InvalidPath,
                    format!("path `{}` crosses a symlink", rel.display()),
                ));
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(ExtensionDataError::new(
                    tau_proto::ExtensionDataErrorKind::NotDir,
                    format!("path ancestor `{}` is not a directory", current.display()),
                ));
            }
            Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(ExtensionDataError::io(
                    format!("failed to stat `{}`", current.display()),
                    error,
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn list_extension_data_entries(
    root: &Path,
    dir: &Path,
) -> Result<Vec<tau_proto::ExtensionDataEntry>, ExtensionDataError> {
    list_extension_data_entries_with_limit(root, dir, MAX_EXTENSION_DATA_LIST_ENTRIES)
}

fn list_extension_data_entries_with_limit(
    root: &Path,
    dir: &Path,
    max_entries: usize,
) -> Result<Vec<tau_proto::ExtensionDataEntry>, ExtensionDataError> {
    let entries = std::fs::read_dir(dir).map_err(|error| {
        ExtensionDataError::io(format!("failed to list `{}`", dir.display()), error)
    })?;
    let mut out = Vec::new();
    for (seen_entries, entry) in entries.enumerate() {
        if max_entries <= seen_entries {
            return Err(quota_exceeded(format!(
                "directory `{}` has more than {max_entries} entries",
                dir.display()
            )));
        }
        let entry = entry
            .map_err(|error| ExtensionDataError::io("failed to read directory entry", error))?;
        let file_type = entry.file_type().map_err(|error| {
            ExtensionDataError::io(
                format!("failed to stat `{}`", entry.path().display()),
                error,
            )
        })?;
        if file_type.is_symlink() || (!file_type.is_file() && !file_type.is_dir()) {
            continue;
        }
        let metadata = entry.metadata().map_err(|error| {
            ExtensionDataError::io(
                format!("failed to stat `{}`", entry.path().display()),
                error,
            )
        })?;
        let rel = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| {
                ExtensionDataError::new(
                    tau_proto::ExtensionDataErrorKind::Io,
                    format!("failed to relativize listed entry: {error}"),
                )
            })?
            .to_string_lossy()
            .into_owned();
        out.push(tau_proto::ExtensionDataEntry {
            path: tau_proto::ExtensionDataPath::new(rel),
            is_dir: metadata.is_dir(),
            len: metadata.is_file().then_some(metadata.len()),
        });
    }
    out.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
    Ok(out)
}

fn quota_exceeded(message: impl Into<String>) -> ExtensionDataError {
    ExtensionDataError::new(tau_proto::ExtensionDataErrorKind::QuotaExceeded, message)
}

fn ensure_request_contents_within_limit(
    contents: &[u8],
    max_bytes: u64,
) -> Result<(), ExtensionDataError> {
    if max_bytes < contents.len() as u64 {
        return Err(quota_exceeded(format!(
            "extension data write is {} bytes; limit is {max_bytes} bytes",
            contents.len()
        )));
    }
    Ok(())
}

fn ensure_file_len_within_limit(
    rel: &Path,
    len: u64,
    max_bytes: u64,
) -> Result<(), ExtensionDataError> {
    if max_bytes < len {
        return Err(quota_exceeded(format!(
            "`{}` is {len} bytes; limit is {max_bytes} bytes",
            rel.display()
        )));
    }
    Ok(())
}

fn create_private_dir_all(path: &Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(path)?;
    set_private_dir_permissions(path)
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, path_std_fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(unix)]
fn open_private_create_new(path: &Path) -> Result<std::fs::File, std::io::Error> {
    use std::os::unix::fs::OpenOptionsExt as _;
    path_std_fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_create_new(path: &Path) -> Result<std::fs::File, std::io::Error> {
    path_std_fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

fn write_file_sync(mut file: std::fs::File, contents: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write as _;
    file.write_all(contents)?;
    file.sync_all()
}

fn sync_parent_dir(path: &Path) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        path_std_fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn finish_secret_mutation<T>(
    mutation: Result<T, std::io::Error>,
    path: &Path,
    state_dir: &Path,
) -> Result<T, std::io::Error> {
    finish_secret_mutation_with(mutation, path, state_dir, |directory| {
        path_std_fs::File::open(directory)?.sync_all()
    })
}

fn finish_secret_mutation_with<T>(
    mutation: Result<T, std::io::Error>,
    path: &Path,
    state_dir: &Path,
    mut sync_directory: impl FnMut(&Path) -> Result<(), std::io::Error>,
) -> Result<T, std::io::Error> {
    let value = mutation?;
    let mut directory = path
        .parent()
        .ok_or_else(|| path_std_io::Error::other("secret path has no containing hierarchy"))?;
    loop {
        sync_directory(directory)?;
        if directory == state_dir {
            return Ok(value);
        }
        directory = directory.parent().ok_or_else(|| {
            path_std_io::Error::other("secret path is outside the Tau state directory")
        })?;
    }
}

fn extension_data_temp_path(path: &Path) -> std::path::PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let nonce = path_std_time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    parent.join(format!(".{name}.tmp-{}-{nonce}", std::process::id()))
}

pub(super) fn create_extension_data_file(
    path: &Path,
    contents: &[u8],
) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    let tmp = extension_data_temp_path(path);
    let mut linked = false;
    let result = (|| {
        let file = open_private_create_new(&tmp)?;
        write_file_sync(file, contents)?;
        std::fs::hard_link(&tmp, path)?;
        linked = true;
        std::fs::remove_file(&tmp)?;
        sync_parent_dir(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        if linked {
            let _ = std::fs::remove_file(path);
            let _ = sync_parent_dir(path);
        }
    }
    result
}

pub(super) fn append_extension_data_file(
    path: &Path,
    contents: &[u8],
) -> Result<(), std::io::Error> {
    append_extension_data_file_with(path, contents, write_file_sync, sync_parent_dir)
}

fn append_extension_data_file_with(
    path: &Path,
    contents: &[u8],
    write_and_sync: impl FnOnce(std::fs::File, &[u8]) -> Result<(), std::io::Error>,
    sync_new_file_parent: impl FnOnce(&Path) -> Result<(), std::io::Error>,
) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    let existed = path.exists();
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        path_std_fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let file = path_std_fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    write_and_sync(file, contents)?;
    if !existed {
        sync_new_file_parent(path)?;
    }
    Ok(())
}

pub(super) fn atomic_replace_extension_data_file(
    path: &Path,
    contents: &[u8],
) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    let tmp = extension_data_temp_path(path);
    let result = (|| {
        let file = open_private_create_new(&tmp)?;
        write_file_sync(file, contents)?;
        std::fs::rename(&tmp, path)?;
        sync_parent_dir(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub(super) fn rename_extension_data_file(from: &Path, to: &Path) -> Result<(), std::io::Error> {
    if let Some(parent) = to.parent() {
        create_private_dir_all(parent)?;
    }
    std::fs::rename(from, to)?;
    sync_parent_dir(to)?;
    sync_parent_dir(from)
}

/// Atomically renames a non-Secret data file only when the destination is
/// absent.
pub(super) fn rename_extension_data_file_noreplace(
    from: &Path,
    to: &Path,
) -> Result<(), std::io::Error> {
    if let Some(parent) = to.parent() {
        create_private_dir_all(parent)?;
    }
    rename_noreplace(from, to)?;
    sync_parent_dir(to)?;
    sync_parent_dir(from)
}

#[cfg(target_os = "linux")]
fn rename_noreplace(from: &Path, to: &Path) -> Result<(), std::io::Error> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};

    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(path_std_io::Error::from)
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(_from: &Path, _to: &Path) -> Result<(), std::io::Error> {
    Err(path_std_io::Error::new(
        path_std_io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable on this platform",
    ))
}

pub(super) fn delete_extension_data_file(path: &Path) -> Result<(), std::io::Error> {
    std::fs::remove_file(path)?;
    sync_parent_dir(path)
}

pub(super) fn run_extension_data_read_file(
    root: &Path,
    path: String,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    run_extension_data_read_file_with_limit(root, path, EXTENSION_DATA_MAX_FILE_BYTES)
}

/// Reads one file while enforcing the selected scope's whole-file limit.
pub(super) fn run_extension_data_read_file_with_limit(
    root: &Path,
    path: String,
    max_bytes: u64,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    let rel = sanitize_extension_data_path(&path, false)?;
    let path = checked_extension_data_path(root, &rel, false)?;
    if !path.is_file() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::NotFile,
            format!("`{}` is not a file", rel.display()),
        ));
    }
    let mut file = path_std_fs::File::open(&path).map_err(|error| {
        ExtensionDataError::io(format!("failed to open `{}`", rel.display()), error)
    })?;
    let mut contents = Vec::new();
    file.by_ref()
        .take(max_bytes + 1)
        .read_to_end(&mut contents)
        .map_err(|error| {
            ExtensionDataError::io(format!("failed to read `{}`", rel.display()), error)
        })?;
    ensure_file_len_within_limit(&rel, contents.len() as u64, max_bytes)?;
    Ok(tau_proto::ExtensionDataValue::ReadFile { contents })
}

pub(super) fn run_extension_data_write_file(
    root: &Path,
    path: String,
    contents: Vec<u8>,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    write_extension_data_file_with_limit(root, path, contents, EXTENSION_DATA_MAX_FILE_BYTES)
}

fn write_extension_data_file_with_limit(
    root: &Path,
    path: String,
    contents: Vec<u8>,
    max_bytes: u64,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    ensure_request_contents_within_limit(&contents, max_bytes)?;
    let rel = sanitize_extension_data_path(&path, false)?;
    let path = checked_extension_data_path(root, &rel, true)?;
    if path.exists() && !path.is_file() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::NotFile,
            format!("`{}` is not a file", rel.display()),
        ));
    }
    atomic_replace_extension_data_file(&path, &contents).map_err(|error| {
        ExtensionDataError::io(format!("failed to write `{}`", rel.display()), error)
    })?;
    Ok(tau_proto::ExtensionDataValue::WriteFile)
}

/// Replaces one Secret file while enforcing its whole-file limit and durable
/// containing-directory publication contract.
pub(super) fn run_extension_data_write_file_with_limit(
    state_dir: &Path,
    root: &Path,
    path: String,
    contents: Vec<u8>,
    max_bytes: u64,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    write_extension_data_file_with_limit_locked(state_dir, root, path, contents, max_bytes)
}

/// Replace one complete file while the caller holds the Secret-scope lock.
pub(super) fn write_extension_data_file_with_limit_locked(
    state_dir: &Path,
    root: &Path,
    path: String,
    contents: Vec<u8>,
    max_bytes: u64,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    write_extension_data_file_with_limit_locked_with(
        state_dir,
        root,
        path,
        contents,
        max_bytes,
        |directory| path_std_fs::File::open(directory)?.sync_all(),
    )
}

fn write_extension_data_file_with_limit_locked_with(
    state_dir: &Path,
    root: &Path,
    path: String,
    contents: Vec<u8>,
    max_bytes: u64,
    sync_directory: impl FnMut(&Path) -> Result<(), std::io::Error>,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    ensure_request_contents_within_limit(&contents, max_bytes)?;
    let rel = sanitize_extension_data_path(&path, false)?;
    let path = checked_extension_data_path(root, &rel, true)?;
    if path.exists() && !path.is_file() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::NotFile,
            format!("`{}` is not a file", rel.display()),
        ));
    }
    finish_secret_mutation_with(
        atomic_replace_extension_data_file(&path, &contents),
        &path,
        state_dir,
        sync_directory,
    )
    .map_err(|error| {
        ExtensionDataError::io(format!("failed to write `{}`", rel.display()), error)
    })?;
    Ok(tau_proto::ExtensionDataValue::WriteFile)
}

pub(super) fn run_extension_data_create_file(
    root: &Path,
    path: String,
    contents: Vec<u8>,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    create_extension_data_file_with_limit(root, path, contents, EXTENSION_DATA_MAX_FILE_BYTES)
}

fn create_extension_data_file_with_limit(
    root: &Path,
    path: String,
    contents: Vec<u8>,
    max_bytes: u64,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    ensure_request_contents_within_limit(&contents, max_bytes)?;
    let rel = sanitize_extension_data_path(&path, false)?;
    let path = checked_extension_data_path(root, &rel, true)?;
    if path.exists() && !path.is_file() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::NotFile,
            format!("`{}` is not a file", rel.display()),
        ));
    }
    create_extension_data_file(&path, &contents).map_err(|error| {
        ExtensionDataError::io(format!("failed to create `{}`", rel.display()), error)
    })?;
    Ok(tau_proto::ExtensionDataValue::CreateFile)
}

/// Creates one Secret file while enforcing its whole-file limit and durable
/// containing-directory publication contract.
pub(super) fn run_extension_data_create_file_with_limit(
    state_dir: &Path,
    root: &Path,
    path: String,
    contents: Vec<u8>,
    max_bytes: u64,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    ensure_request_contents_within_limit(&contents, max_bytes)?;
    let rel = sanitize_extension_data_path(&path, false)?;
    let path = checked_extension_data_path(root, &rel, true)?;
    if path.exists() && !path.is_file() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::NotFile,
            format!("`{}` is not a file", rel.display()),
        ));
    }
    finish_secret_mutation(
        create_extension_data_file(&path, &contents),
        &path,
        state_dir,
    )
    .map_err(|error| {
        ExtensionDataError::io(format!("failed to create `{}`", rel.display()), error)
    })?;
    Ok(tau_proto::ExtensionDataValue::CreateFile)
}

pub(super) fn run_extension_data_append_file(
    root: &Path,
    path: String,
    contents: Vec<u8>,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    ensure_request_contents_within_limit(&contents, EXTENSION_DATA_MAX_FILE_BYTES)?;
    let rel = sanitize_extension_data_path(&path, false)?;
    let path = checked_extension_data_path(root, &rel, true)?;
    if path.exists() && !path.is_file() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::NotFile,
            format!("`{}` is not a file", rel.display()),
        ));
    }
    if path.exists() {
        let metadata = std::fs::metadata(&path).map_err(|error| {
            ExtensionDataError::io(format!("failed to stat `{}`", rel.display()), error)
        })?;
        let appended_len = metadata.len().saturating_add(contents.len() as u64);
        ensure_file_len_within_limit(&rel, appended_len, EXTENSION_DATA_MAX_FILE_BYTES)?;
    }
    append_extension_data_file(&path, &contents).map_err(|error| {
        ExtensionDataError::io(format!("failed to append `{}`", rel.display()), error)
    })?;
    Ok(tau_proto::ExtensionDataValue::AppendFile)
}

/// Appends one file while serializing the complete append operation across
/// harness processes sharing the same extension-data scope root.
pub(super) fn run_locked_extension_data_append_file(
    root: &Path,
    path: String,
    contents: Vec<u8>,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    with_extension_data_scope_lock(root, || {
        run_extension_data_append_file(root, path, contents)
    })
}

/// Selects the approved append locking policy for one extension-data scope.
pub(super) fn run_scoped_extension_data_append_file(
    scope: tau_proto::ExtensionDataScope,
    root: &Path,
    path: String,
    contents: Vec<u8>,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    match scope {
        tau_proto::ExtensionDataScope::Session | tau_proto::ExtensionDataScope::User => {
            run_locked_extension_data_append_file(root, path, contents)
        }
        tau_proto::ExtensionDataScope::Cache | tau_proto::ExtensionDataScope::Secret => {
            run_extension_data_append_file(root, path, contents)
        }
    }
}

/// Atomically replaces one complete file when its current BLAKE3 generation
/// matches `expected_generation`.
///
/// Locking the scope directory serializes comparison and replacement across
/// harness processes sharing the same state root. The replacement itself
/// retains the normal synchronous file and parent-directory durability
/// contract.
pub(super) fn run_extension_data_compare_and_swap_file(
    state_dir: &Path,
    root: &Path,
    path: String,
    expected_generation: String,
    contents: Vec<u8>,
    max_bytes: u64,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    ensure_request_contents_within_limit(&contents, max_bytes)?;
    let rel = sanitize_extension_data_path(&path, false)?;
    let root_file = lock_extension_data_scope(root)?;
    let result = (|| {
        let path = checked_extension_data_path(root, &rel, false)?;
        let mut current = Vec::new();
        open_read_no_follow(&path)
            .and_then(|mut file| {
                file.by_ref()
                    .take(max_bytes + 1)
                    .read_to_end(&mut current)
                    .map(|_| ())
            })
            .map_err(|error| {
                ExtensionDataError::io(format!("failed to read `{}`", rel.display()), error)
            })?;
        ensure_file_len_within_limit(&rel, current.len() as u64, max_bytes)?;
        let actual_generation = blake3::hash(&current).to_hex().to_string();
        if actual_generation != expected_generation {
            return Err(ExtensionDataError::new(
                tau_proto::ExtensionDataErrorKind::GenerationMismatch,
                format!("`{}` changed since it was read", rel.display()),
            ));
        }
        finish_secret_mutation(
            atomic_replace_extension_data_file(&path, &contents),
            &path,
            state_dir,
        )
        .map_err(|error| {
            ExtensionDataError::io(format!("failed to write `{}`", rel.display()), error)
        })?;
        Ok(tau_proto::ExtensionDataValue::CompareAndSwapFile)
    })();
    let _ = fs2::FileExt::unlock(&root_file);
    result
}

/// Serializes one complete scope mutation against CAS and setup-side writers.
pub(super) fn with_extension_data_scope_lock<T>(
    root: &Path,
    operation: impl FnOnce() -> Result<T, ExtensionDataError>,
) -> Result<T, ExtensionDataError> {
    let root_file = lock_extension_data_scope(root)?;
    let result = operation();
    let _ = fs2::FileExt::unlock(&root_file);
    result
}

fn lock_extension_data_scope(root: &Path) -> Result<std::fs::File, ExtensionDataError> {
    use fs2::FileExt as _;

    let _ = checked_extension_data_path(root, Path::new(""), true)?;
    let root_file = open_directory_no_follow(root)
        .map_err(|error| ExtensionDataError::io("failed to open data scope", error))?;
    root_file
        .lock_exclusive()
        .map_err(|error| ExtensionDataError::io("failed to lock data scope", error))?;
    Ok(root_file)
}

#[cfg(unix)]
fn open_directory_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    path_std_fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_directory_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    path_std_fs::File::open(path)
}

#[cfg(unix)]
fn open_read_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    path_std_fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_read_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    path_std_fs::File::open(path)
}

pub(super) fn run_extension_data_delete_file(
    root: &Path,
    path: String,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    let rel = sanitize_extension_data_path(&path, false)?;
    let path = checked_extension_data_path(root, &rel, true)?;
    if path.exists() && !path.is_file() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::NotFile,
            format!("`{}` is not a file", rel.display()),
        ));
    }
    match delete_extension_data_file(&path) {
        Ok(()) => Ok(tau_proto::ExtensionDataValue::DeleteFile),
        Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => {
            Ok(tau_proto::ExtensionDataValue::DeleteFile)
        }
        Err(error) => Err(ExtensionDataError::io(
            format!("failed to delete `{}`", rel.display()),
            error,
        )),
    }
}

/// Deletes one Secret file and durably publishes every containing Tau-owned
/// directory entry before reporting success.
pub(super) fn run_secret_data_delete_file(
    state_dir: &Path,
    root: &Path,
    path: String,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    let rel = sanitize_extension_data_path(&path, false)?;
    let result = run_extension_data_delete_file(root, path)?;
    finish_secret_mutation(Ok(result), &root.join(rel), state_dir)
        .map_err(|error| ExtensionDataError::io("failed to publish secret deletion", error))
}

pub(super) fn run_extension_data_rename_file(
    root: &Path,
    from: String,
    to: String,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    run_extension_data_rename_file_with(root, from, to, rename_extension_data_file_noreplace)
}

/// Read only the standard reporter file while holding the User-scope append
/// lock; absent storage does not create a new reporter directory.
pub(super) fn run_papercut_read(
    root: &Path,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    if !root
        .try_exists()
        .map_err(|error| ExtensionDataError::io("failed to inspect papercut storage", error))?
    {
        // A dangling symlink must fail rather than masquerade as absent.
        if path_std_fs::symlink_metadata(root)
            .is_err_and(|error| error.kind() == path_std_io::ErrorKind::NotFound)
        {
            return Ok(tau_proto::ExtensionDataValue::ReadPapercuts { contents: None });
        }
    }
    with_extension_data_scope_lock(root, || {
        let rel = Path::new(tau_proto::PAPERCUT_FILE_NAME);
        let file = checked_extension_data_path(root, rel, true)?;
        match path_std_fs::symlink_metadata(&file) {
            Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => {
                Ok(tau_proto::ExtensionDataValue::ReadPapercuts { contents: None })
            }
            Err(error) => Err(ExtensionDataError::io(
                "failed to inspect papercut records",
                error,
            )),
            Ok(_) => Ok(tau_proto::ExtensionDataValue::ReadPapercuts {
                contents: Some(read_papercut_bytes(&file)?),
            }),
        }
    })
}

/// Read one checked reporter file without following its final symlink.
fn read_papercut_bytes(file: &Path) -> Result<Vec<u8>, ExtensionDataError> {
    let mut file = open_read_no_follow(file)
        .map_err(|error| ExtensionDataError::io("failed to open papercut records", error))?;
    let metadata = file
        .metadata()
        .map_err(|error| ExtensionDataError::io("failed to inspect papercut records", error))?;
    if !metadata.is_file() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::NotFile,
            "papercut records are not a regular file",
        ));
    }
    let mut contents = Vec::new();
    file.by_ref()
        .take(EXTENSION_DATA_MAX_FILE_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|error| ExtensionDataError::io("failed to read papercut records", error))?;
    ensure_file_len_within_limit(
        Path::new(tau_proto::PAPERCUT_FILE_NAME),
        contents.len() as u64,
        EXTENSION_DATA_MAX_FILE_BYTES,
    )?;
    Ok(contents)
}

/// Archive a previously validated reporter snapshot only if no append or
/// replacement changed its bytes in the meantime. The lock covers the complete
/// generation check, collision selection, no-replace rename, and directory
/// sync.
pub(super) fn run_papercut_archive(
    root: &Path,
    expected_generation: &str,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    if expected_generation.len() != 64
        || !expected_generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::InvalidPath,
            "invalid papercut snapshot generation",
        ));
    }
    with_extension_data_scope_lock(root, || {
        let file =
            checked_extension_data_path(root, Path::new(tau_proto::PAPERCUT_FILE_NAME), false)?;
        let contents = read_papercut_bytes(&file)?;
        if blake3::hash(&contents).to_hex().as_str() != expected_generation {
            return Err(ExtensionDataError::new(
                tau_proto::ExtensionDataErrorKind::GenerationMismatch,
                "papercut records changed since they were validated",
            ));
        }
        for index in 1_u64.. {
            let name = format!("{}{index:016}.jsonl", tau_proto::PAPERCUT_ARCHIVE_PREFIX);
            let archive = root.join(&name);
            match path_std_fs::symlink_metadata(&archive) {
                Ok(_) => continue,
                Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(ExtensionDataError::io(
                        "failed to inspect papercut archive path",
                        error,
                    ));
                }
            }
            match rename_extension_data_file_noreplace(&file, &archive) {
                Ok(()) => {
                    return Ok(tau_proto::ExtensionDataValue::ArchivePapercuts {
                        archive: tau_proto::ExtensionDataPath::new(name),
                    });
                }
                Err(error) if error.kind() == path_std_io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(ExtensionDataError::io(
                        "failed to archive papercut records",
                        error,
                    ));
                }
            }
        }
        unreachable!("u64 archive namespace cannot be exhausted in practice")
    })
}

/// Validates a rename request and applies the selected scope-specific rename
/// operation.
pub(super) fn run_extension_data_rename_file_with(
    root: &Path,
    from: String,
    to: String,
    rename_file: impl FnOnce(&Path, &Path) -> Result<(), std::io::Error>,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    let from_rel = sanitize_extension_data_path(&from, false)?;
    let to_rel = sanitize_extension_data_path(&to, false)?;
    let from = checked_extension_data_path(root, &from_rel, false)?;
    let to = checked_extension_data_path(root, &to_rel, true)?;
    if !from.is_file() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::NotFile,
            format!("`{}` is not a file", from_rel.display()),
        ));
    }
    if to.exists() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::AlreadyExists,
            format!("`{}` already exists", to_rel.display()),
        ));
    }
    rename_file(&from, &to).map_err(|error| {
        ExtensionDataError::io(
            format!(
                "failed to rename `{}` to `{}`",
                from_rel.display(),
                to_rel.display()
            ),
            error,
        )
    })?;
    Ok(tau_proto::ExtensionDataValue::RenameFile)
}

/// Renames one Secret file and durably publishes both containing Tau-owned
/// directory hierarchies before reporting success.
pub(super) fn run_secret_data_rename_file(
    state_dir: &Path,
    root: &Path,
    from: String,
    to: String,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    let from_rel = sanitize_extension_data_path(&from, false)?;
    let to_rel = sanitize_extension_data_path(&to, false)?;
    let result = run_extension_data_rename_file_with(root, from, to, rename_extension_data_file)?;
    finish_secret_mutation(Ok(()), &root.join(from_rel), state_dir)
        .and_then(|()| finish_secret_mutation(Ok(result), &root.join(to_rel), state_dir))
        .map_err(|error| ExtensionDataError::io("failed to publish secret rename", error))
}

pub(super) fn run_extension_data_list_files(
    root: &Path,
    path: String,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    let rel = sanitize_extension_data_path(&path, true)?;
    let dir = checked_extension_data_path(root, &rel, true)?;
    if !dir.exists() {
        return Ok(tau_proto::ExtensionDataValue::ListFiles {
            entries: Vec::new(),
        });
    }
    if !dir.is_dir() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::NotDir,
            format!("`{}` is not a directory", rel.display()),
        ));
    }
    let entries = list_extension_data_entries(root, &dir)?;
    Ok(tau_proto::ExtensionDataValue::ListFiles { entries })
}

#[cfg(test)]
mod tests;
