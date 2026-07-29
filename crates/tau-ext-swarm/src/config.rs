use std::collections::HashSet;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use iroh::{EndpointAddr, EndpointId, RelayUrl, TransportAddr};
use serde::Deserialize;
use tau_proto::SecretValue;
use tau_swarm_client::Backoff;
use tau_swarm_client_api::{Credential, CredentialId, Secret};

/// Strict operator configuration for one Swarm extension instance.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtConfig {
    /// Pinned server identity and optional reachability hints.
    pub endpoint: EndpointConfig,
    /// Public worker credential identifier.
    pub credential_id: String,
    /// Name of the Configure-provided credential secret.
    pub credential_secret: String,
    /// Stable published hostname, or the UTF-8 system hostname when omitted.
    #[serde(default)]
    pub hostname: Option<String>,
    /// Reconnection policy.
    #[serde(default)]
    pub reconnect: ReconnectConfig,
    /// End-to-end local command deadline in milliseconds.
    #[serde(default = "default_command_timeout_ms")]
    pub command_timeout_ms: u64,
    /// Process-memory admission limits.
    #[serde(default)]
    pub limits: Limits,
}

/// Pinned Iroh endpoint and optional route hints.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EndpointConfig {
    /// Canonical Iroh endpoint public identity.
    pub peer_id: String,
    /// Optional relay route hint.
    #[serde(default)]
    pub relay_url: Option<String>,
    /// Optional direct socket route hints.
    #[serde(default)]
    pub direct_addresses: Vec<String>,
}

/// Bounded reconnect policy.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ReconnectConfig {
    /// Initial retry delay in milliseconds.
    pub initial_delay_ms: u64,
    /// Maximum retry delay in milliseconds.
    pub maximum_delay_ms: u64,
    /// Symmetric jitter in per-mille.
    pub jitter_per_mille: u16,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial_delay_ms: 250,
            maximum_delay_ms: 30_000,
            jitter_per_mille: 200,
        }
    }
}

impl ReconnectConfig {
    /// Builds the approved reconnect policy from validated values and an
    /// OS-random nonzero seed.
    #[must_use]
    pub fn backoff(&self, seed: u64) -> Backoff {
        Backoff::new(
            Duration::from_millis(self.initial_delay_ms),
            Duration::from_millis(self.maximum_delay_ms),
            self.jitter_per_mille,
            seed,
        )
    }
}

/// Configurable retained-state and local queue bounds.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Limits {
    /// Total retained prompt and blocker-answer command IDs.
    pub command_entries: usize,
    /// Logical bytes retained by commands.
    pub command_bytes: usize,
    /// Total blocker history entries.
    pub blocker_entries: usize,
    /// Maximum encoded full-history blocker result bytes.
    pub blocker_bytes: usize,
    /// Unacknowledged update entries.
    pub update_entries: usize,
    /// Logical bytes retained by updates.
    pub update_bytes: usize,
    /// Retained projection changes.
    pub change_history_entries: usize,
    /// Logical bytes retained by changes.
    pub change_history_bytes: usize,
    /// Maximum encoded snapshot or individual change bytes.
    pub publication_bytes: usize,
    /// Maximum current agents.
    pub agent_entries: usize,
    /// Maximum current watch memberships.
    pub watch_entries: usize,
    /// Capacity of each local Tau-submission queue.
    pub submission_queue_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            command_entries: 1_024,
            command_bytes: 16 * 1024 * 1024,
            blocker_entries: 256,
            blocker_bytes: 4 * 1024 * 1024,
            update_entries: 256,
            update_bytes: 8 * 1024 * 1024,
            change_history_entries: 4_096,
            change_history_bytes: 32 * 1024 * 1024,
            publication_bytes: 8 * 1024 * 1024,
            agent_entries: 4_096,
            watch_entries: 16_384,
            submission_queue_entries: 16,
        }
    }
}

/// Validated runtime configuration. It deliberately implements neither
/// `Debug` nor serialization because it owns the resolved secret.
#[derive(Clone)]
pub(crate) struct ResolvedConfig {
    /// Server endpoint including the single pinned identity.
    pub endpoint: EndpointAddr,
    /// Optional configured relay to insert into the live N0 relay map.
    pub relay: Option<RelayUrl>,
    /// Worker authentication credential.
    pub credential: Credential,
    /// Published stable host identity.
    pub hostname: String,
    /// Reconnect timing parameters.
    pub reconnect: ReconnectConfig,
    /// Local command deadline.
    pub command_timeout: Duration,
    /// Process-memory limits.
    pub limits: Limits,
}

impl ExtConfig {
    /// Validates configuration and resolves its Configure-provided secret.
    pub fn resolve(
        self,
        secrets: &std::collections::BTreeMap<String, SecretValue>,
    ) -> Result<ResolvedConfig, String> {
        validate_len_controls("credential_id", &self.credential_id, 1, 128)?;
        if 128 < self.endpoint.peer_id.len() {
            return Err("endpoint.peer_id exceeds 128 bytes".into());
        }
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: silent-map-err
        let peer_id = EndpointId::from_str(&self.endpoint.peer_id)
            .map_err(|_| "endpoint.peer_id is not a valid Iroh EndpointId")?;
        let relay = self
            .endpoint
            .relay_url
            .as_deref()
            .map(|text| {
                if 2_048 < text.len() {
                    return Err(String::from("endpoint.relay_url exceeds 2048 bytes"));
                }
                // Preserve this behavior; the structural alternative is not semantics-neutral
                // here. ast-grep-ignore: silent-map-err
                text.parse::<RelayUrl>()
                    .map_err(|_| String::from("endpoint.relay_url is invalid"))
            })
            .transpose()?;
        if 16 < self.endpoint.direct_addresses.len() {
            return Err("endpoint.direct_addresses exceeds 16 entries".into());
        }
        let mut direct = HashSet::new();
        for text in &self.endpoint.direct_addresses {
            // Preserve this behavior; the structural alternative is not semantics-neutral
            // here. ast-grep-ignore: silent-map-err
            let address = text
                .parse::<SocketAddr>()
                .map_err(|_| "endpoint.direct_addresses contains an invalid socket address")?;
            if !direct.insert(address) {
                return Err("endpoint.direct_addresses contains a duplicate".into());
            }
        }
        validate_reconnect(&self.reconnect)?;
        in_range("command_timeout_ms", self.command_timeout_ms, 1_000, 25_000)?;
        self.limits.validate()?;
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: match-option-verbose
        let hostname = match self.hostname {
            Some(hostname) => hostname,
            None => system_hostname()?,
        };
        validate_hostname(&hostname)?;
        let secret = secrets
            .get(&self.credential_secret)
            .ok_or_else(|| format!("missing Configure secret {:?}", self.credential_secret))?
            .expose_secret();
        if secret.is_empty() || 4_096 < secret.len() {
            return Err("credential secret must contain 1..=4096 bytes".into());
        }
        let transports = relay
            .iter()
            .cloned()
            .map(TransportAddr::Relay)
            .chain(direct.into_iter().map(TransportAddr::Ip));
        Ok(ResolvedConfig {
            endpoint: EndpointAddr::from_parts(peer_id, transports),
            relay,
            credential: Credential {
                id: CredentialId::new(self.credential_id),
                secret: Secret::new(secret.as_bytes()),
            },
            hostname,
            reconnect: self.reconnect,
            command_timeout: Duration::from_millis(self.command_timeout_ms),
            limits: self.limits,
        })
    }
}

impl Limits {
    fn validate(&self) -> Result<(), String> {
        range_usize("limits.command_entries", self.command_entries, 1, 16_384)?;
        range_usize(
            "limits.command_bytes",
            self.command_bytes,
            1,
            256 * 1024 * 1024,
        )?;
        range_usize("limits.blocker_entries", self.blocker_entries, 1, 4_096)?;
        range_usize(
            "limits.blocker_bytes",
            self.blocker_bytes,
            256 * 1024,
            4 * 1024 * 1024,
        )?;
        range_usize("limits.update_entries", self.update_entries, 1, 4_096)?;
        range_usize(
            "limits.update_bytes",
            self.update_bytes,
            256 * 1024,
            64 * 1024 * 1024,
        )?;
        range_usize(
            "limits.change_history_entries",
            self.change_history_entries,
            1,
            65_536,
        )?;
        range_usize(
            "limits.change_history_bytes",
            self.change_history_bytes,
            1024 * 1024,
            128 * 1024 * 1024,
        )?;
        range_usize(
            "limits.publication_bytes",
            self.publication_bytes,
            1024 * 1024,
            8 * 1024 * 1024,
        )?;
        range_usize("limits.agent_entries", self.agent_entries, 1, 65_536)?;
        range_usize("limits.watch_entries", self.watch_entries, 1, 262_144)?;
        range_usize(
            "limits.submission_queue_entries",
            self.submission_queue_entries,
            1,
            64,
        )
    }
}

fn default_command_timeout_ms() -> u64 {
    25_000
}

fn validate_reconnect(value: &ReconnectConfig) -> Result<(), String> {
    in_range(
        "reconnect.initial_delay_ms",
        value.initial_delay_ms,
        10,
        60_000,
    )?;
    in_range(
        "reconnect.maximum_delay_ms",
        value.maximum_delay_ms,
        10,
        300_000,
    )?;
    if value.maximum_delay_ms < value.initial_delay_ms {
        return Err("reconnect.initial_delay_ms must be <= reconnect.maximum_delay_ms".into());
    }
    in_range(
        "reconnect.jitter_per_mille",
        u64::from(value.jitter_per_mille),
        0,
        1_000,
    )
}

fn in_range(name: &str, value: u64, min: u64, max: u64) -> Result<(), String> {
    if value < min || max < value {
        Err(format!("{name} is outside {min}..={max}"))
    } else {
        Ok(())
    }
}
fn range_usize(name: &str, value: usize, min: usize, max: usize) -> Result<(), String> {
    if value < min || max < value {
        Err(format!("{name} is outside {min}..={max}"))
    } else {
        Ok(())
    }
}
fn validate_len_controls(name: &str, value: &str, min: usize, max: usize) -> Result<(), String> {
    if value.len() < min || max < value.len() || value.chars().any(char::is_control) {
        Err(format!("{name} has invalid length or control characters"))
    } else {
        Ok(())
    }
}
fn validate_hostname(value: &str) -> Result<(), String> {
    if value.is_empty() || 255 < value.len() || !value.is_ascii() {
        return Err("hostname must contain 1..=255 ASCII bytes".into());
    }
    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_alphanumeric()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
        || bytes
            .iter()
            .any(|b| !b.is_ascii_alphanumeric() && !matches!(b, b'.' | b'_' | b'-'))
    {
        return Err("hostname has invalid syntax".into());
    }
    Ok(())
}
fn system_hostname() -> Result<String, String> {
    let mut bytes = [0_u8; 256];
    // SAFETY: the writable fixed buffer and its exact capacity are valid.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) };
    if rc != 0 {
        return Err("system hostname lookup failed".into());
    }
    let len = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: silent-map-err
    std::str::from_utf8(&bytes[..len])
        .map(str::to_owned)
        .map_err(|_| "system hostname is not UTF-8".into())
}

#[cfg(test)]
mod tests;
