//! Shared helpers for owning one Telegram Bot API update stream.
//!
//! Telegram exposes one `getUpdates` stream per Bot API base URL plus bot
//! token. This module keeps the local ownership mechanics independent from the
//! legacy extension runtime so gateway code can reuse the same lock scope,
//! token redaction, webhook preflight messages, and 409 diagnostics.
//! Bot API long polling is the chosen inbound transport; see
//! `DECISION-tau-ext-telegram-long-polling`.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;

use crate::LOG_TARGET;

/// Maximum user-visible bytes copied from Telegram diagnostics.
const MAX_DIAGNOSTIC_TEXT_BYTES: usize = 1024;

/// Sensitive identity inputs for one Telegram Bot API update stream.
///
/// This type carries the raw bot token so it must not be logged, formatted, or
/// exposed in diagnostics. Use [`StreamIdentity::fingerprint`] when a stable
/// non-secret stream identifier is needed, and [`StreamIdentity::redact_token`]
/// before showing Telegram response text.
pub(crate) struct StreamIdentity<'a> {
    /// Bot API base URL without a trailing slash.
    api_base: &'a str,
    /// Raw bot token, used only as hash/redaction input.
    bot_token: &'a str,
}

impl<'a> StreamIdentity<'a> {
    /// Build a sensitive stream identity from validated runtime configuration
    /// fields.
    pub(crate) fn new(api_base: &'a str, bot_token: &'a str) -> Self {
        Self {
            api_base,
            bot_token,
        }
    }

    /// Return the non-secret Bot API base URL.
    pub(crate) fn api_base(&self) -> &str {
        self.api_base
    }

    /// Build a stable non-secret fingerprint for the singleton Bot API stream.
    pub(crate) fn fingerprint(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"tau-ext-telegram update stream lock v1\0");
        hasher.update(self.api_base.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.bot_token.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    /// Redact this stream's bot token from a diagnostic string.
    pub(crate) fn redact_token(&self, text: &str) -> String {
        redact_token(text, self.bot_token)
    }
}

/// Telegram webhook state relevant to long-poll ownership.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TelegramWebhookInfo {
    /// Configured webhook URL, empty when the bot is in getUpdates mode.
    pub(crate) url: String,
    /// Telegram's reported number of pending updates, if present.
    pub(crate) pending_update_count: Option<i64>,
    /// Last webhook delivery error, if Telegram reported one.
    pub(crate) last_error_message: Option<String>,
}

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
    /// Try to acquire the lock for `identity` under the shared Tau
    /// extension-lock root.
    ///
    /// `state_dir` must be one owner's extension instance state directory below
    /// the shared Tau extension state root. Lock files are stored under
    /// `<state_dir.parent()>/telegram-update-stream-locks/` so the legacy local
    /// poller and the future gateway owner contend in the same filesystem
    /// scope when they use the same Tau state root.
    ///
    /// The raw bot token is included only in the stream fingerprint input and
    /// is never written to the filesystem or returned in diagnostics.
    pub(crate) fn acquire(state_dir: &Path, identity: StreamIdentity<'_>) -> Result<Self, String> {
        let locks_dir = lock_root(state_dir)?;
        fs::create_dir_all(&locks_dir)
            .map_err(|e| format!("creating Telegram update-stream lock directory: {e}"))?;
        let stream_hash = identity.fingerprint();
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
                    identity.api_base(),
                    stream_hash,
                    path.display(),
                    owner.trim()
                ));
            }
            return Err(format!("locking Telegram update stream: {error}"));
        }
        write_owner_metadata(&mut file, &identity, &stream_hash)
            .map_err(|e| format!("writing Telegram update-stream lock metadata: {e}"))?;
        Ok(Self {
            file,
            path,
            stream_hash,
        })
    }

    /// Return whether this lock covers `identity`.
    pub(crate) fn covers(&self, identity: StreamIdentity<'_>) -> bool {
        self.stream_hash == identity.fingerprint()
    }
}

impl Drop for UpdateStreamLock {
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        let _ = FileExt::unlock(&self.file);
        tracing::debug!(
            target: LOG_TARGET,
            lock = %self.path.display(),
            stream_hash = %self.stream_hash,
            "released Telegram update-stream lock"
        );
    }
}

/// Remove the configured bot token from Telegram response text.
fn redact_token(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_owned()
    } else {
        text.replace(token, "<redacted>")
    }
}

/// Build the fail-closed message used when Telegram has an active webhook.
pub(crate) fn webhook_active_message(info: &TelegramWebhookInfo) -> String {
    let mut message = "Telegram bot has an active webhook, so getUpdates polling cannot be used. \
                       Tau did not delete the webhook or drop updates; remove the webhook yourself \
                       or configure a different bot token."
        .to_owned();
    if let Some(count) = info.pending_update_count {
        message.push_str(&format!(" Telegram reports {count} pending update(s)."));
    }
    if let Some(error) = info
        .last_error_message
        .as_deref()
        .filter(|error| !error.trim().is_empty())
    {
        message.push_str(" Last webhook error: ");
        message.push_str(&bounded_diagnostic_text(error));
    }
    message
}

/// Classify Telegram HTTP 409 errors into actionable stream-owner diagnostics.
pub(crate) fn telegram_contention_diagnostic(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    if !lower.contains("http 409") && !lower.contains("conflict") {
        return None;
    }
    if lower.contains("webhook") {
        return Some(
            "Telegram getUpdates returned HTTP 409 because a webhook is active or was changed. \
             Tau stopped Telegram polling for this registration; it did not delete the webhook \
             or drop updates. Remove the webhook yourself or configure a different bot token."
                .to_owned(),
        );
    }
    if lower.contains("getupdates") || lower.contains("bot instance") {
        return Some(
            "Telegram getUpdates returned HTTP 409 because another long-poll consumer is using \
             this bot token. Tau stopped Telegram polling for this registration to avoid racing \
             the singleton update stream; stop the other bot/session or configure a different \
             bot token."
                .to_owned(),
        );
    }
    Some(
        "Telegram getUpdates returned HTTP 409 conflict. Tau stopped Telegram polling for this \
         registration because the bot update stream is not exclusively available."
            .to_owned(),
    )
}

/// Locate a lock root shared by all configured Telegram owners.
fn lock_root(state_dir: &Path) -> Result<PathBuf, String> {
    let ext_root = state_dir.parent().ok_or_else(|| {
        "telegram extension state directory has no parent for shared lock root".to_owned()
    })?;
    Ok(ext_root.join("telegram-update-stream-locks"))
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
    identity: &StreamIdentity<'_>,
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
    writeln!(file, "api_base={}", identity.api_base())?;
    writeln!(file, "stream_hash={stream_hash}")?;
    file.sync_data()
}

/// Bound and sanitize diagnostic text before including it in user-visible
/// messages.
fn bounded_diagnostic_text(text: &str) -> String {
    let mut sanitized = String::new();
    for ch in text.trim().chars() {
        let ch_len = ch.len_utf8();
        if sanitized.len() + ch_len > MAX_DIAGNOSTIC_TEXT_BYTES {
            sanitized.push('…');
            break;
        }
        if ch.is_control() && ch != '\n' && ch != '\t' {
            sanitized.push('�');
        } else {
            sanitized.push(ch);
        }
    }
    sanitized
}

#[cfg(test)]
mod tests;
