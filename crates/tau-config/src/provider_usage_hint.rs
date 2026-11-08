//! Advisory startup hints, deliberately separate from live quota authority.

use std::fs::{DirBuilder, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tau_proto::{ProviderName, ProviderQuotaWindow};

/// Maximum sidecar size accepted by the startup reader.
const MAX_BYTES: u64 = 64 * 1024;

/// Minimal nonsecret account observations used only to choose startup aliases.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderUsageHint {
    /// Sidecar format version, not a provider epoch.
    version: u8,
    /// Configured provider namespace, never an upstream account identity.
    provider: ProviderName,
    /// All account constraints; losing any constraint makes the score unknown.
    windows: Vec<UsageWindow>,
}

/// One percentage and its conservative local validity interval.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UsageWindow {
    /// Provider-reported percentage in basis points.
    used: u16,
    /// Local Unix milliseconds of the usage observation.
    observed_ms: u64,
    /// Exclusive expiry; zero represents invalid timing.
    expires_ms: u64,
}

impl ProviderUsageHint {
    /// Copies accepted nonempty canonical windows without route or credential
    /// data.
    pub fn from_windows(provider: ProviderName, windows: &[ProviderQuotaWindow]) -> Option<Self> {
        if windows.is_empty() {
            return None;
        }
        Some(Self {
            version: 1,
            provider,
            windows: windows.iter().map(UsageWindow::from_window).collect(),
        })
    }

    /// Returns bottleneck headroom, or unknown if any constraint has expired.
    pub fn remaining(&self, now_ms: u64) -> Option<u16> {
        if self.version != 1
            || self.windows.is_empty()
            || self.windows.len() > tau_proto::MAX_PROVIDER_QUOTA_WINDOWS
        {
            return None;
        }
        self.windows.iter().try_fold(10_000, |remaining, window| {
            if window.used > 10_000
                || window.observed_ms > now_ms
                || window.expires_ms <= now_ms
                || window.expires_ms <= window.observed_ms
            {
                return None;
            }
            Some(remaining.min(10_000 - window.used))
        })
    }

    /// Reads one bounded advisory file; every failure is an unknown candidate.
    pub fn read(state_dir: &Path, provider: &ProviderName) -> Option<Self> {
        let file = File::open(hint_path(state_dir, provider)).ok()?;
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > MAX_BYTES {
            return None;
        }
        let hint: Self = serde_json::from_slice(&bytes).ok()?;
        (hint.version == 1 && hint.provider == *provider).then_some(hint)
    }

    /// Atomically replaces a private per-provider file, on a filesystem worker.
    pub fn write(&self, state_dir: &Path) -> std::io::Result<()> {
        let path = hint_path(state_dir, &self.provider);
        let parent = path.parent().expect("hint path has parent");
        let mut builder = DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
        let permissions = {
            #[cfg(unix)]
            {
                use std::fs::Permissions;
                use std::os::unix::fs::PermissionsExt;
                Some(Permissions::from_mode(0o600))
            }
            #[cfg(not(unix))]
            {
                None
            }
        };
        crate::atomic::atomic_write_following_symlink(
            &path,
            &serde_json::to_vec(self)?,
            permissions,
        )
    }
}

impl UsageWindow {
    /// Bounds retention by one observed duration and every supplied reset
    /// clock.
    fn from_window(window: &ProviderQuotaWindow) -> Self {
        let observed_ms = window
            .usage_observed_at_unix_ms
            .get()
            .max(
                window
                    .timing_anchor_observed_at_unix_ms
                    .map_or(0, |time| time.get()),
            )
            .max(
                window
                    .server_offset_observed_at_unix_ms
                    .map_or(0, |time| time.get()),
            );
        let expires_ms = expiry(window).unwrap_or(0);
        Self {
            used: window.used_basis_points,
            observed_ms,
            expires_ms,
        }
    }
}

/// Computes conservative expiry without synthesizing usage after a reset.
fn expiry(window: &ProviderQuotaWindow) -> Option<u64> {
    let observed = window.usage_observed_at_unix_ms.get();
    let mut expires = observed.checked_add(window.window_seconds.get().checked_mul(1000)?)?;
    match (
        window.remaining_seconds_at_timing_anchor,
        window.timing_anchor_observed_at_unix_ms,
    ) {
        (Some(remaining), Some(anchor)) => {
            expires = expires.min(
                anchor
                    .get()
                    .checked_add(u64::try_from(remaining.get()).ok()?.checked_mul(1000)?)?,
            );
        }
        (None, None) => {}
        _ => return None,
    }
    if let Some(reset) = window.reset_at_unix_seconds {
        let offset = window.server_offset_ms.map_or(0, |offset| offset.get());
        let local = i128::from(reset.get()) * 1000 - i128::from(offset);
        expires = expires.min(u64::try_from(local).ok()?);
    }
    Some(expires)
}

/// Encodes namespace bytes so provider names cannot escape the sidecar
/// directory.
fn hint_path(state_dir: &Path, provider: &ProviderName) -> PathBuf {
    use std::fmt::Write;
    let mut name = String::new();
    for byte in provider.as_str().as_bytes() {
        write!(&mut name, "{byte:02x}").expect("string writes cannot fail");
    }
    state_dir
        .join("provider-usage")
        .join(format!("{name}.json"))
}

#[cfg(test)]
mod tests;
