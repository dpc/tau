//! Bounded legacy capture inventory, deliberately not an attempt ledger.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Read as _;
use std::path::Path;

use serde_json::Value;
use tau_proto::{AgentPromptId, SessionId};
use zstd::stream::read::Decoder;

use super::CacheScanLimits;

/// Counts of independently observed files, never inferred dispatch counts.
#[derive(Default, serde::Serialize)]
pub(super) struct CaptureCounts {
    /// Files containing a supported request envelope.
    pub request_files: u64,
    /// Files containing a supported successful-response envelope.
    pub response_files: u64,
    /// Files containing a supported failure envelope.
    pub failure_files: u64,
    /// Recognized scalar files, not reconstructed attempts or canonical joins.
    pub diagnostic_files: u64,
}

/// Content-free evidence collected without retaining provider payloads or IDs.
#[derive(Default)]
pub(super) struct Inventory {
    /// Exact typed session/prompt attribution; no terminal association implied.
    pub prompts: BTreeMap<(SessionId, AgentPromptId), CaptureCounts>,
    /// Fixed reason codes and encountered occurrence counts.
    pub gaps: BTreeMap<&'static str, u64>,
    /// Total decoded bytes consumed, including rejected files.
    decoded: u64,
}

impl Inventory {
    /// Counts a gap without retaining error prose or a source pathname.
    pub fn gap(&mut self, reason: &'static str) {
        *self.gaps.entry(reason).or_default() += 1;
    }

    /// Scans only the selected session's existing instance directories.
    pub fn scan(&mut self, root: &Path, session: &SessionId, limits: &CacheScanLimits) {
        if root.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
            self.gap("capture_directory_not_regular");
            return;
        }
        let Ok(instances) = std::fs::read_dir(root) else {
            self.gap("capture_directory_unavailable");
            return;
        };
        for instance in instances {
            let Ok(instance) = instance else {
                self.gap("capture_directory_unreadable");
                continue;
            };
            if !instance.file_type().is_ok_and(|kind| kind.is_dir()) {
                self.gap("capture_instance_not_directory");
                continue;
            }
            let Ok(files) = std::fs::read_dir(instance.path()) else {
                self.gap("capture_directory_unreadable");
                continue;
            };
            for file in files {
                let Ok(file) = file else {
                    self.gap("capture_directory_unreadable");
                    continue;
                };
                if !file.file_name().as_encoded_bytes().ends_with(b".json.zst") {
                    continue;
                }
                if !file.file_type().is_ok_and(|kind| kind.is_file()) {
                    self.gap("capture_not_regular");
                    continue;
                }
                if self.decoded >= limits.total_decompressed_bytes {
                    self.gap("cumulative_capture_limit");
                    return;
                }
                match self.read(&file.path(), limits) {
                    Ok(value) => self.observe(session, value, limits),
                    Err(reason) => self.gap(reason),
                }
            }
        }
    }

    /// Opens one non-symlink file and bounds both decode and JSON allocation.
    fn read(&mut self, path: &Path, limits: &CacheScanLimits) -> Result<Value, &'static str> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file: File = options.open(path).map_err(|_| "capture_unreadable")?;
        let metadata = file.metadata().map_err(|_| "capture_unreadable")?;
        if !metadata.is_file() {
            return Err("capture_not_regular");
        }
        if metadata.len() > limits.compressed_file_bytes {
            return Err("compressed_capture_limit");
        }
        // A deliberately conservative allowance covers serde's tree, keys,
        // scalar allocations and the decoded buffer, including tiny JSON nodes.
        let retained = (self.prompts.len() as u64).saturating_mul(1024);
        let parse_budget = (limits.working_memory_bytes / 8 * 3).saturating_sub(retained) / 128;
        let cap = limits
            .decompressed_file_bytes
            .min(limits.total_decompressed_bytes.saturating_sub(self.decoded))
            .min(parse_budget);
        let mut decoder = Decoder::new(file.take(limits.compressed_file_bytes.saturating_add(1)))
            .map_err(|_| "malformed_compression")?;
        decoder
            .window_log_max((limits.working_memory_bytes / 8).max(1024).ilog2().min(23))
            .map_err(|_| "compression_window_limit")?;
        let mut bytes = Vec::new();
        let result = decoder.take(cap.saturating_add(1)).read_to_end(&mut bytes);
        self.decoded = self.decoded.saturating_add(bytes.len() as u64);
        result.map_err(|_| "truncated_or_malformed_compression")?;
        if bytes.len() as u64 > cap {
            return Err("decoded_or_memory_capture_limit");
        }
        serde_json::from_slice::<super::strict_json::StrictJson>(&bytes)
            .map(|value| value.0)
            .map_err(|_| "malformed_or_ambiguous_capture_json")
    }

    /// Projects only recognized envelopes; content and arbitrary metadata die
    /// here.
    fn observe(&mut self, session: &SessionId, value: Value, limits: &CacheScanLimits) {
        let cache_diagnostic = value.get("schema").and_then(Value::as_str)
            == Some("tau.cache_diagnostic")
            && value.get("schema_version").and_then(Value::as_u64) == Some(0);
        let known_failure = value.get("schema").is_none()
            && matches!(
                (
                    value.get("schema_version").and_then(Value::as_u64),
                    value.get("capture_kind").and_then(Value::as_str)
                ),
                (Some(1), Some("provider_attempt_failure"))
                    | (Some(0), Some("compact_http_failure"))
            );
        if !known_failure
            && !cache_diagnostic
            && (value.get("schema").is_some() || value.get("schema_version").is_some())
        {
            self.gap("unsupported_capture_schema");
            return;
        }
        let Some(captured_session) = value.get("session_id").and_then(Value::as_str) else {
            self.gap("capture_attribution_unavailable");
            return;
        };
        if cache_diagnostic
            && value.get("operation").and_then(Value::as_str) == Some("cache_refresh")
        {
            if captured_session != session.as_str() {
                self.gap("capture_session_mismatch");
                return;
            }
            let valid = cache_diagnostic_header(&value)
                && value.get("agent_prompt_id").is_some_and(Value::is_null)
                && value.get("logical_attempt").is_some_and(Value::is_null)
                && value
                    .get("harness_provider_attempt")
                    .is_some_and(Value::is_null)
                && value
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| tau_proto::AgentId::parse(id).is_ok())
                && value
                    .get("operation_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty() && id.len() <= 128);
            self.gap(if valid {
                "cache_operation_analysis_unavailable"
            } else {
                "malformed_current_cache_diagnostic"
            });
            return;
        }
        let Some(prompt) = value.get("agent_prompt_id").and_then(Value::as_str) else {
            self.gap("capture_attribution_unavailable");
            return;
        };
        let Ok(prompt) = AgentPromptId::parse(prompt) else {
            self.gap("capture_attribution_malformed");
            return;
        };
        if captured_session != session.as_str() {
            self.gap("capture_session_mismatch");
            return;
        }
        if cache_diagnostic && !cache_diagnostic_header(&value) {
            self.gap("malformed_current_cache_diagnostic");
            return;
        }
        if known_failure {
            let valid = match value.get("capture_kind").and_then(Value::as_str) {
                Some("compact_http_failure") => super::failure_shape::compact(&value),
                Some("provider_attempt_failure") => super::failure_shape::attempt(&value),
                _ => false,
            };
            if !valid {
                self.gap("malformed_current_failure_capture");
                return;
            }
        }
        let chat = value.get("backend").and_then(Value::as_str) == Some("chat_completions");
        let chat_response = chat
            && value.get("usage").is_some()
            && value.get("stop_reason").is_some()
            && value.get("output_items").is_some_and(Value::is_array)
            && value.get("raw_events").is_some_and(Value::is_array);
        let chat_http_failure = chat
            && value
                .get("http_status")
                .and_then(Value::as_u64)
                .is_some_and(|status| u16::try_from(status).is_ok())
            && value.get("body").is_some_and(Value::is_string);
        let kind = if cache_diagnostic {
            3
        } else if known_failure || chat_http_failure {
            2
        } else if value.get("body").is_some_and(Value::is_object) {
            0
        } else if chat_response
            || (value.get("provider_response_id").is_some() && value.get("usage").is_some())
        {
            1
        } else if value.get("error").is_some() {
            2
        } else {
            self.gap("unsupported_capture_shape");
            return;
        };
        if (self.prompts.len() as u64)
            .saturating_add(1)
            .saturating_mul(1024)
            > limits.working_memory_bytes / 2
        {
            self.gap("inventory_memory_limit");
            return;
        }
        let counts = self.prompts.entry((session.clone(), prompt)).or_default();
        match kind {
            0 => counts.request_files += 1,
            1 => counts.response_files += 1,
            2 => counts.failure_files += 1,
            _ => counts.diagnostic_files += 1,
        }
        if cache_diagnostic {
            self.gap("cache_diagnostic_analysis_unavailable");
        } else {
            self.gap("legacy_partial");
        }
    }
}

/// Recognize only the minimal current scalar header, without interpreting
/// dispatch facts or promoting inventory to an attempt reader.
fn cache_diagnostic_header(value: &Value) -> bool {
    matches!(
        value.get("record_kind").and_then(Value::as_str),
        Some("dispatch" | "attempt_end")
    ) && value.get("record_seq").and_then(Value::as_u64).is_some()
        && ["attempt_id", "producer_run_id"].iter().all(|field| {
            value.get(field).and_then(Value::as_str).is_some_and(|id| {
                id.len() == 32
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        })
}
