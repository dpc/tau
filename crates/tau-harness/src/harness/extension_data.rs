//! Extension-owned persistent data filesystem helpers.
//!
//! This module keeps the path validation, symlink rejection, and atomic file
//! update rules for `ExtensionDataRequest` outside of the central harness
//! event loop.

use std::io::Read as _;
use std::path::{Path, PathBuf};

/// Maximum bytes accepted for one extension-owned data file.
const MAX_EXTENSION_DATA_FILE_BYTES: u64 = 16 * 1024 * 1024;
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
            std::io::ErrorKind::NotFound => tau_proto::ExtensionDataErrorKind::NotFound,
            std::io::ErrorKind::AlreadyExists => tau_proto::ExtensionDataErrorKind::AlreadyExists,
            std::io::ErrorKind::PermissionDenied => tau_proto::ExtensionDataErrorKind::Permission,
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
            std::path::Component::Normal(part) => out.push(part),
            std::path::Component::CurDir => {
                return Err(ExtensionDataError::new(
                    tau_proto::ExtensionDataErrorKind::InvalidPath,
                    "path must not contain `.`",
                ));
            }
            std::path::Component::ParentDir => {
                return Err(ExtensionDataError::new(
                    tau_proto::ExtensionDataErrorKind::InvalidPath,
                    "path must not contain `..`",
                ));
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
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
        Err(error) if allow_missing_leaf && error.kind() == std::io::ErrorKind::NotFound => {
            Ok(full)
        }
        Err(error) => Err(ExtensionDataError::io(
            format!("failed to stat `{}`", rel.display()),
            error,
        )),
    }
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
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
    let entries = std::fs::read_dir(dir).map_err(|error| {
        ExtensionDataError::io(format!("failed to list `{}`", dir.display()), error)
    })?;
    let mut out = Vec::new();
    for (seen_entries, entry) in entries.enumerate() {
        if MAX_EXTENSION_DATA_LIST_ENTRIES <= seen_entries {
            return Err(quota_exceeded(format!(
                "directory `{}` has more than {MAX_EXTENSION_DATA_LIST_ENTRIES} entries",
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

fn ensure_request_contents_within_limit(contents: &[u8]) -> Result<(), ExtensionDataError> {
    if contents.len() as u64 > MAX_EXTENSION_DATA_FILE_BYTES {
        return Err(quota_exceeded(format!(
            "extension data write is {} bytes; limit is {MAX_EXTENSION_DATA_FILE_BYTES} bytes",
            contents.len()
        )));
    }
    Ok(())
}

fn ensure_file_len_within_limit(rel: &Path, len: u64) -> Result<(), ExtensionDataError> {
    if MAX_EXTENSION_DATA_FILE_BYTES < len {
        return Err(quota_exceeded(format!(
            "`{}` is {len} bytes; limit is {MAX_EXTENSION_DATA_FILE_BYTES} bytes",
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
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(unix)]
fn open_private_create_new(path: &Path) -> Result<std::fs::File, std::io::Error> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_create_new(path: &Path) -> Result<std::fs::File, std::io::Error> {
    std::fs::OpenOptions::new()
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
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn extension_data_temp_path(path: &Path) -> std::path::PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let nonce = std::time::SystemTime::now()
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
        // This call is intentionally best-effort; preserve the existing discarded
        // result. ast-grep-ignore: let-underscore-call
        let _ = std::fs::remove_file(&tmp);
        if linked {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = std::fs::remove_file(path);
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = sync_parent_dir(path);
        }
    }
    result
}

pub(super) fn append_extension_data_file(
    path: &Path,
    contents: &[u8],
) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    let existed = path.exists();
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    write_file_sync(file, contents)?;
    if !existed {
        sync_parent_dir(path)?;
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
        // This call is intentionally best-effort; preserve the existing discarded
        // result. ast-grep-ignore: let-underscore-call
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

pub(super) fn delete_extension_data_file(path: &Path) -> Result<(), std::io::Error> {
    std::fs::remove_file(path)?;
    sync_parent_dir(path)
}

pub(super) fn run_extension_data_read_file(
    root: &Path,
    path: String,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    let rel = sanitize_extension_data_path(&path, false)?;
    let path = checked_extension_data_path(root, &rel, false)?;
    if !path.is_file() {
        return Err(ExtensionDataError::new(
            tau_proto::ExtensionDataErrorKind::NotFile,
            format!("`{}` is not a file", rel.display()),
        ));
    }
    let mut file = std::fs::File::open(&path).map_err(|error| {
        ExtensionDataError::io(format!("failed to open `{}`", rel.display()), error)
    })?;
    let mut contents = Vec::new();
    file.by_ref()
        .take(MAX_EXTENSION_DATA_FILE_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|error| {
            ExtensionDataError::io(format!("failed to read `{}`", rel.display()), error)
        })?;
    ensure_file_len_within_limit(&rel, contents.len() as u64)?;
    Ok(tau_proto::ExtensionDataValue::ReadFile { contents })
}

pub(super) fn run_extension_data_write_file(
    root: &Path,
    path: String,
    contents: Vec<u8>,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    ensure_request_contents_within_limit(&contents)?;
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

pub(super) fn run_extension_data_create_file(
    root: &Path,
    path: String,
    contents: Vec<u8>,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    ensure_request_contents_within_limit(&contents)?;
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

pub(super) fn run_extension_data_append_file(
    root: &Path,
    path: String,
    contents: Vec<u8>,
) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
    ensure_request_contents_within_limit(&contents)?;
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
        ensure_file_len_within_limit(&rel, appended_len)?;
    }
    append_extension_data_file(&path, &contents).map_err(|error| {
        ExtensionDataError::io(format!("failed to append `{}`", rel.display()), error)
    })?;
    Ok(tau_proto::ExtensionDataValue::AppendFile)
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
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(tau_proto::ExtensionDataValue::DeleteFile)
        }
        Err(error) => Err(ExtensionDataError::io(
            format!("failed to delete `{}`", rel.display()),
            error,
        )),
    }
}

pub(super) fn run_extension_data_rename_file(
    root: &Path,
    from: String,
    to: String,
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
    rename_extension_data_file(&from, &to).map_err(|error| {
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
