//! Advisory OS lock for one Telegram Bot API update stream.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;

use crate::RuntimeConfig;

/// Held exclusive advisory lock for one Telegram update stream.
pub(crate) struct UpdateStreamLock {
    /// Open file descriptor carrying the operating-system advisory lock.
    file: File,
    /// Filesystem path of the locked sidecar file.
    path: PathBuf,
    /// Non-secret stream fingerprint used in diagnostics and metadata.
    stream_hash: String,
}

impl UpdateStreamLock {
    /// Try to acquire the lock for `cfg` under the shared Tau extension-lock
    /// root.
    ///
    /// The raw bot token is included only in the stream fingerprint input
    /// and is never written to the filesystem or returned in diagnostics.
    pub(crate) fn acquire(state_dir: &Path, cfg: &RuntimeConfig) -> Result<Self, String> {
        let locks_dir = lock_root(state_dir)?;
        fs::create_dir_all(&locks_dir)
            .map_err(|e| format!("creating Telegram update-stream lock directory: {e}"))?;
        let stream_hash = stream_hash(cfg);
        let path = locks_dir.join(format!("{stream_hash}.lock"));
        let mut file = open_lock_file(&path)
            .map_err(|e| format!("opening Telegram update-stream lock: {e}"))?;
        if let Err(error) = FileExt::try_lock_exclusive(&file) {
            if error.kind() == ErrorKind::WouldBlock {
                let owner = read_owner_metadata(&mut file)
                    .filter(|owner| !owner.trim().is_empty())
                    .unwrap_or_else(|| "owner metadata unavailable".to_owned());
                return Err(format!(
                    "Telegram update stream is already locked by another Tau process \
                     (api_base={}, stream_hash={}, lock={}, owner: {})",
                    cfg.api_base,
                    stream_hash,
                    path.display(),
                    owner.trim()
                ));
            }
            return Err(format!("locking Telegram update stream: {error}"));
        }
        write_owner_metadata(&mut file, cfg, &stream_hash)
            .map_err(|e| format!("writing Telegram update-stream lock metadata: {e}"))?;
        Ok(Self {
            file,
            path,
            stream_hash,
        })
    }

    /// Return whether this lock covers the update stream described by `cfg`.
    pub(crate) fn covers(&self, cfg: &RuntimeConfig) -> bool {
        self.stream_hash == stream_hash(cfg)
    }
}

impl Drop for UpdateStreamLock {
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        let _ = FileExt::unlock(&self.file);
        tracing::debug!(
            target: crate::LOG_TARGET,
            lock = %self.path.display(),
            stream_hash = %self.stream_hash,
            "released Telegram update-stream lock"
        );
    }
}

/// Locate a lock root shared by all configured instances of this extension.
fn lock_root(state_dir: &Path) -> Result<PathBuf, String> {
    let ext_root = state_dir.parent().ok_or_else(|| {
        "telegram extension state directory has no parent for shared lock root".to_owned()
    })?;
    Ok(ext_root.join("telegram-update-stream-locks"))
}

/// Build a stable non-secret fingerprint for the singleton Bot API stream.
fn stream_hash(cfg: &RuntimeConfig) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tau-ext-telegram update stream lock v1\0");
    hasher.update(cfg.api_base.as_bytes());
    hasher.update(b"\0");
    hasher.update(cfg.bot_token.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Open the lock file using private permissions where the platform supports it.
fn open_lock_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Best-effort read of the current lock owner metadata.
fn read_owner_metadata(file: &mut File) -> Option<String> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut text).ok()?;
    Some(text)
}

/// Write non-secret metadata useful when another process reports contention.
fn write_owner_metadata(
    file: &mut File,
    cfg: &RuntimeConfig,
    stream_hash: &str,
) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "pid={}", std::process::id())?;
    if let Ok(exe) = std::env::current_exe() {
        writeln!(file, "exe={}", exe.display())?;
    }
    let acquired_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    writeln!(file, "acquired_unix={acquired_unix}")?;
    writeln!(file, "api_base={}", cfg.api_base)?;
    writeln!(file, "stream_hash={stream_hash}")?;
    file.sync_data()
}
