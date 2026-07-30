//! Shared filename contract for session provider debug captures.

#[cfg(test)]
mod tests;

/// Valid request/response and transport combination for one provider capture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderDebugCaptureClass {
    /// HTTP/SSE request metadata.
    HttpSseRequest,
    /// Responses WebSocket request metadata.
    WebsocketRequest,
    /// HTTP/SSE response or HTTP-error metadata.
    HttpSseResponse,
    /// Responses WebSocket response metadata.
    WebsocketResponse,
    /// Response metadata whose transport descriptor is unavailable.
    UnknownResponse,
    /// Bounded, redacted metadata for one failed finite Responses attempt.
    ResponsesAttemptFailure,
}

impl ProviderDebugCaptureClass {
    /// Return the stable filename label for this class.
    fn label(self) -> &'static str {
        match self {
            Self::HttpSseRequest => "http-sse-request",
            Self::WebsocketRequest => "websocket-request",
            Self::HttpSseResponse => "http-sse-response",
            Self::WebsocketResponse => "websocket-response",
            Self::UnknownResponse => "unknown-response",
            Self::ResponsesAttemptFailure => "responses-attempt-failure",
        }
    }
}

/// On-disk encoding represented by one provider capture filename.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderDebugCaptureFormat {
    /// Historical uncompressed JSON.
    LegacyJson,
    /// New zstd-compressed JSON.
    ZstdJson,
}

impl ProviderDebugCaptureFormat {
    /// Return the exact filename extension.
    fn extension(self) -> &'static str {
        match self {
            Self::LegacyJson => ".json",
            Self::ZstdJson => ".json.zst",
        }
    }
}

/// Validated provider capture basename shared by writers and retention cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderDebugCaptureFilename {
    /// Complete safe basename.
    basename: String,
}

impl ProviderDebugCaptureFilename {
    /// Construct one timestamped basename from validated protocol identity.
    #[must_use]
    pub fn new(
        timestamp_micros: u128,
        agent_prompt_id: &tau_proto::AgentPromptId,
        class: ProviderDebugCaptureClass,
        format: ProviderDebugCaptureFormat,
    ) -> Self {
        Self {
            basename: format!(
                "{timestamp_micros}-{}-{}{}",
                agent_prompt_id.as_str(),
                class.label(),
                format.extension()
            ),
        }
    }

    /// Parse an exact legacy or compressed provider capture basename.
    #[must_use]
    pub fn parse(basename: &str) -> Option<Self> {
        let (stem, format) = basename
            .strip_suffix(".json.zst")
            .map(|stem| (stem, ProviderDebugCaptureFormat::ZstdJson))
            .or_else(|| {
                basename
                    .strip_suffix(".json")
                    .map(|stem| (stem, ProviderDebugCaptureFormat::LegacyJson))
            })?;
        let (prefix, class) = [
            ProviderDebugCaptureClass::HttpSseRequest,
            ProviderDebugCaptureClass::WebsocketRequest,
            ProviderDebugCaptureClass::HttpSseResponse,
            ProviderDebugCaptureClass::WebsocketResponse,
            ProviderDebugCaptureClass::UnknownResponse,
            ProviderDebugCaptureClass::ResponsesAttemptFailure,
        ]
        .into_iter()
        .find_map(|class| {
            stem.strip_suffix(class.label())
                .and_then(|prefix| prefix.strip_suffix('-'))
                .map(|prefix| (prefix, class))
        })?;
        let (timestamp, agent_prompt_id) = prefix.split_once('-')?;
        if timestamp.is_empty() || !timestamp.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let timestamp_micros = timestamp.parse().ok()?;
        let agent_prompt_id = tau_proto::AgentPromptId::parse(agent_prompt_id).ok()?;
        let filename = Self::new(timestamp_micros, &agent_prompt_id, class, format);
        (filename.as_str() == basename).then_some(filename)
    }

    /// Return the safe basename.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.basename
    }
}
