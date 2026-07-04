use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tau_proto::{
    ExtensionDataErrorKind, ExtensionDataPath, ExtensionDataRequestOp, ExtensionDataScope,
    ExtensionDataValue,
};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) type SharedStorage = Rc<dyn Storage>;

pub(crate) trait Storage {
    fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, String>;
    fn write_file(&self, path: &str, contents: Vec<u8>) -> Result<(), String>;
    fn create_file(&self, path: &str, contents: Vec<u8>) -> Result<(), StorageCreateError>;
    fn append_file(&self, path: &str, contents: Vec<u8>) -> Result<(), String>;
    fn delete_file(&self, path: &str) -> Result<(), String>;
    fn list_files(&self, path: &str) -> Result<Vec<StorageEntry>, String>;

    fn file_exists(&self, path: &str) -> Result<bool, String> {
        self.read_file(path).map(|contents| contents.is_some())
    }
}

#[derive(Debug)]
pub(crate) enum StorageCreateError {
    AlreadyExists,
    Other(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StorageEntry {
    pub(crate) path: String,
    pub(crate) is_dir: bool,
}

pub(crate) struct FsStorage {
    root: PathBuf,
}

impl FsStorage {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    fn create_parent_dir(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            self.harden_dir_hierarchy(parent)?;
        }
        Ok(())
    }

    fn harden_dir_hierarchy(&self, path: &Path) -> Result<(), String> {
        if self.root.exists() {
            self.harden_dir(&self.root)?;
        }
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return self.harden_dir(path);
        };
        let mut current = self.root.clone();
        for component in relative.components() {
            current.push(component.as_os_str());
            self.harden_dir(&current)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn harden_dir(&self, path: &Path) -> Result<(), String> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())
    }

    #[cfg(not(unix))]
    fn harden_dir(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }

    #[cfg(unix)]
    fn harden_file(&self, path: &Path) -> Result<(), String> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())
    }

    #[cfg(not(unix))]
    fn harden_file(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }

    fn sync_parent_dir(&self, path: &Path) -> Result<(), String> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        let dir = std::fs::File::open(parent).map_err(|error| error.to_string())?;
        dir.sync_all().map_err(|error| error.to_string())
    }

    fn temp_path(&self, path: &Path) -> Result<PathBuf, String> {
        let parent = path
            .parent()
            .ok_or_else(|| "storage path has no parent".to_owned())?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "storage path has no file name".to_owned())?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        Ok(parent.join(format!(
            ".{name}.tmp-pim-{}-{timestamp}-{}",
            std::process::id(),
            NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn write_private_temp(&self, path: &Path, contents: Vec<u8>) -> Result<(), String> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(path).map_err(|error| error.to_string())?;
        file.write_all(&contents)
            .map_err(|error| error.to_string())?;
        self.harden_file(path)?;
        file.sync_all().map_err(|error| error.to_string())
    }
}

impl Storage for FsStorage {
    fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        let path = self.path(path);
        match std::fs::read(&path) {
            Ok(contents) => {
                if let Some(parent) = path.parent() {
                    self.harden_dir_hierarchy(parent)?;
                }
                self.harden_file(&path)?;
                Ok(Some(contents))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn write_file(&self, path: &str, contents: Vec<u8>) -> Result<(), String> {
        let path = self.path(path);
        self.create_parent_dir(&path)?;
        let temp = self.temp_path(&path)?;
        self.write_private_temp(&temp, contents)?;
        if let Err(error) = std::fs::rename(&temp, &path) {
            let _ = std::fs::remove_file(&temp);
            return Err(error.to_string());
        }
        self.harden_file(&path)?;
        self.sync_parent_dir(&path)
    }

    fn create_file(&self, path: &str, contents: Vec<u8>) -> Result<(), StorageCreateError> {
        let path = self.path(path);
        self.create_parent_dir(&path)
            .map_err(StorageCreateError::Other)?;
        let temp = self.temp_path(&path).map_err(StorageCreateError::Other)?;
        self.write_private_temp(&temp, contents)
            .map_err(StorageCreateError::Other)?;
        match std::fs::hard_link(&temp, &path) {
            Ok(()) => {
                std::fs::remove_file(&temp)
                    .map_err(|error| StorageCreateError::Other(error.to_string()))?;
                self.harden_file(&path).map_err(StorageCreateError::Other)?;
                self.sync_parent_dir(&path)
                    .map_err(StorageCreateError::Other)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&temp);
                Err(StorageCreateError::AlreadyExists)
            }
            Err(error) => {
                let _ = std::fs::remove_file(&temp);
                Err(StorageCreateError::Other(error.to_string()))
            }
        }
    }

    fn append_file(&self, path: &str, contents: Vec<u8>) -> Result<(), String> {
        let path = self.path(path);
        let existed = path.exists();
        self.create_parent_dir(&path)?;
        let mut options = std::fs::OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&path).map_err(|error| error.to_string())?;
        self.harden_file(&path)?;
        file.write_all(&contents)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        if !existed {
            self.sync_parent_dir(&path)?;
        }
        Ok(())
    }

    fn delete_file(&self, path: &str) -> Result<(), String> {
        let path = self.path(path);
        match std::fs::remove_file(&path) {
            Ok(()) => self.sync_parent_dir(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn list_files(&self, path: &str) -> Result<Vec<StorageEntry>, String> {
        let dir = self.path(path);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        self.harden_dir_hierarchy(&dir)?;
        let mut entries = std::fs::read_dir(&dir)
            .map_err(|error| error.to_string())?
            .map(|entry| {
                let entry = entry.map_err(|error| error.to_string())?;
                let is_dir = entry
                    .file_type()
                    .map_err(|error| error.to_string())?
                    .is_dir();
                let name = entry.file_name().to_string_lossy().into_owned();
                Ok(StorageEntry {
                    path: if path.is_empty() {
                        name
                    } else {
                        format!("{path}/{name}")
                    },
                    is_dir,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }
}

/// Storage backend that proxies PIM state operations through harness
/// extension-data RPC.
pub(crate) struct RpcStorage {
    /// Extension-data scope used for every PIM storage operation.
    scope: ExtensionDataScope,
    /// Correlated tau-client RPC helper shared with the manual runtime loop.
    client: tau_client::ExtensionDataClient,
}

impl RpcStorage {
    pub(crate) fn new(scope: ExtensionDataScope, client: tau_client::ExtensionDataClient) -> Self {
        Self { scope, client }
    }

    fn request(&self, op: ExtensionDataRequestOp) -> Result<ExtensionDataValue, String> {
        match self.client.request(self.scope.clone(), op) {
            Ok(value) => Ok(value),
            Err(tau_client::ExtensionDataRpcError::Harness { kind, message }) => {
                Err(format_storage_error(kind, message))
            }
            Err(tau_client::ExtensionDataRpcError::Disconnect(_))
            | Err(tau_client::ExtensionDataRpcError::InputClosed) => {
                Err("harness disconnected during extension data request".to_owned())
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

impl Storage for RpcStorage {
    fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        match self.request(ExtensionDataRequestOp::ReadFile {
            path: ExtensionDataPath::new(path),
        }) {
            Ok(ExtensionDataValue::ReadFile { contents }) => Ok(Some(contents)),
            Ok(other) => Err(format!("unexpected extension data read result: {other:?}")),
            Err(message) if message.starts_with("not_found:") => Ok(None),
            Err(message) => Err(message),
        }
    }

    fn write_file(&self, path: &str, contents: Vec<u8>) -> Result<(), String> {
        match self.request(ExtensionDataRequestOp::WriteFile {
            path: ExtensionDataPath::new(path),
            contents,
        })? {
            ExtensionDataValue::WriteFile => Ok(()),
            other => Err(format!("unexpected extension data write result: {other:?}")),
        }
    }

    fn create_file(&self, path: &str, contents: Vec<u8>) -> Result<(), StorageCreateError> {
        match self.request(ExtensionDataRequestOp::CreateFile {
            path: ExtensionDataPath::new(path),
            contents,
        }) {
            Ok(ExtensionDataValue::CreateFile) => Ok(()),
            Ok(other) => Err(StorageCreateError::Other(format!(
                "unexpected extension data create result: {other:?}"
            ))),
            Err(message) if message.starts_with("already_exists:") => {
                Err(StorageCreateError::AlreadyExists)
            }
            Err(message) => Err(StorageCreateError::Other(message)),
        }
    }

    fn append_file(&self, path: &str, contents: Vec<u8>) -> Result<(), String> {
        match self.request(ExtensionDataRequestOp::AppendFile {
            path: ExtensionDataPath::new(path),
            contents,
        })? {
            ExtensionDataValue::AppendFile => Ok(()),
            other => Err(format!(
                "unexpected extension data append result: {other:?}"
            )),
        }
    }

    fn delete_file(&self, path: &str) -> Result<(), String> {
        match self.request(ExtensionDataRequestOp::DeleteFile {
            path: ExtensionDataPath::new(path),
        })? {
            ExtensionDataValue::DeleteFile => Ok(()),
            other => Err(format!(
                "unexpected extension data delete result: {other:?}"
            )),
        }
    }

    fn list_files(&self, path: &str) -> Result<Vec<StorageEntry>, String> {
        match self.request(ExtensionDataRequestOp::ListFiles {
            path: ExtensionDataPath::new(path),
        })? {
            ExtensionDataValue::ListFiles { entries } => Ok(entries
                .into_iter()
                .map(|entry| StorageEntry {
                    path: entry.path.into_string(),
                    is_dir: entry.is_dir,
                })
                .collect()),
            other => Err(format!("unexpected extension data list result: {other:?}")),
        }
    }
}

fn format_storage_error(kind: ExtensionDataErrorKind, message: String) -> String {
    format!(
        "{}:{message}",
        match kind {
            ExtensionDataErrorKind::NotFound => "not_found",
            ExtensionDataErrorKind::AlreadyExists => "already_exists",
            ExtensionDataErrorKind::InvalidPath => "invalid_path",
            ExtensionDataErrorKind::NotFile => "not_file",
            ExtensionDataErrorKind::NotDir => "not_dir",
            ExtensionDataErrorKind::Permission => "permission",
            ExtensionDataErrorKind::QuotaExceeded => "quota_exceeded",
            ExtensionDataErrorKind::Io => "io",
        }
    )
}

pub(crate) fn file_name(path: &str) -> Option<&str> {
    Path::new(path).file_name().and_then(|name| name.to_str())
}
