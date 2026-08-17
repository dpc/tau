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
        let stem = basename.strip_suffix(".json.zst")?;
        let (prefix, class) = [
            ProviderDebugCaptureClass::HttpSseRequest,
            ProviderDebugCaptureClass::WebsocketRequest,
            ProviderDebugCaptureClass::HttpSseResponse,
            ProviderDebugCaptureClass::WebsocketResponse,
            ProviderDebugCaptureClass::UnknownResponse,
            ProviderDebugCaptureClass::ResponsesAttemptFailure,
            ProviderDebugCaptureClass::CompactHttpFailure,
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
