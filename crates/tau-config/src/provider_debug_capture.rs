//! Shared filename contract for session provider debug captures.

#[cfg(test)]
mod tests;

pub use tau_proto::ProviderDebugCaptureClass;

/// Return the stable filename label owned by this grammar module.
fn class_label(class: ProviderDebugCaptureClass) -> &'static str {
    match class {
        ProviderDebugCaptureClass::HttpSseRequest => "http-sse-request",
        ProviderDebugCaptureClass::WebsocketRequest => "websocket-request",
        ProviderDebugCaptureClass::HttpSseResponse => "http-sse-response",
        ProviderDebugCaptureClass::WebsocketResponse => "websocket-response",
        ProviderDebugCaptureClass::UnknownResponse => "unknown-response",
        ProviderDebugCaptureClass::ResponsesAttemptFailure => "responses-attempt-failure",
        ProviderDebugCaptureClass::CompactHttpFailure => "compact-http-failure",
        ProviderDebugCaptureClass::CacheDiagnostic => "cache-diagnostic",
    }
}

/// Validated provider capture basename shared by writers and retention cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderDebugCaptureFilename {
    /// Complete safe basename.
    basename: String,
}

impl ProviderDebugCaptureFilename {
    /// Construct an operation-only scalar filename, distinct from prompt
    /// grammar.
    #[must_use]
    pub fn cache_operation(timestamp_micros: u128, id: tau_proto::CacheOperationId) -> Self {
        Self {
            basename: format!(
                "{timestamp_micros}.cache-operation.{}.cache-diagnostic.json.zst",
                id.to_hex()
            ),
        }
    }

    /// Validate the closed class/attribution pairing before choosing a
    /// basename.
    pub fn attributed(
        timestamp_micros: u128,
        attribution: &tau_proto::ProviderCaptureAttribution,
        class: ProviderDebugCaptureClass,
    ) -> Option<Self> {
        if !attribution.permits(class) {
            return None;
        }
        Some(match attribution {
            tau_proto::ProviderCaptureAttribution::Prompt(id) => {
                Self::new(timestamp_micros, id, class)
            }
            tau_proto::ProviderCaptureAttribution::CacheOperation(id) => {
                Self::cache_operation(timestamp_micros, *id)
            }
        })
    }
    /// Construct one timestamped basename from validated protocol identity.
    #[must_use]
    pub fn new(
        timestamp_micros: u128,
        agent_prompt_id: &tau_proto::AgentPromptId,
        class: ProviderDebugCaptureClass,
    ) -> Self {
        Self {
            basename: format!(
                "{timestamp_micros}-{}-{}.json.zst",
                agent_prompt_id.as_str(),
                class_label(class),
            ),
        }
    }

    /// Parse an exact compressed provider capture basename.
    #[must_use]
    pub fn parse(basename: &str) -> Option<Self> {
        if let Some((timestamp, rest)) = basename.split_once(".cache-operation.") {
            let id = rest.strip_suffix(".cache-diagnostic.json.zst")?;
            let id = tau_proto::CacheOperationId::parse(id)?;
            let filename = Self::cache_operation(timestamp.parse().ok()?, id);
            return (filename.as_str() == basename).then_some(filename);
        }
        let stem = basename.strip_suffix(".json.zst")?;
        let (prefix, class) = [
            ProviderDebugCaptureClass::HttpSseRequest,
            ProviderDebugCaptureClass::WebsocketRequest,
            ProviderDebugCaptureClass::HttpSseResponse,
            ProviderDebugCaptureClass::WebsocketResponse,
            ProviderDebugCaptureClass::UnknownResponse,
            ProviderDebugCaptureClass::ResponsesAttemptFailure,
            ProviderDebugCaptureClass::CompactHttpFailure,
            ProviderDebugCaptureClass::CacheDiagnostic,
        ]
        .into_iter()
        .find_map(|class| {
            stem.strip_suffix(class_label(class))
                .and_then(|prefix| prefix.strip_suffix('-'))
                .map(|prefix| (prefix, class))
        })?;
        let (timestamp, agent_prompt_id) = prefix.split_once('-')?;
        if timestamp.is_empty() || !timestamp.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let timestamp_micros = timestamp.parse().ok()?;
        let agent_prompt_id = tau_proto::AgentPromptId::parse(agent_prompt_id).ok()?;
        let filename = Self::new(timestamp_micros, &agent_prompt_id, class);
        (filename.as_str() == basename).then_some(filename)
    }

    /// Return the safe basename.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.basename
    }
}
