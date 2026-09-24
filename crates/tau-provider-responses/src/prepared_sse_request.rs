//! Provider-owned request lowering over the shared finite SSE attempt.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::{AttemptConfig, AttemptModel, Error, build_request};

/// Exact, fully lowered full-replay request for the shared SSE transport.
///
/// This is a library seam for provider adapters, not a user-configurable
/// request override. The owning adapter must apply its route's request policy
/// and the normal destination-origin admission before constructing this value.
/// The shared transport and output parser retain their existing behavior.
/// The attempt's prompt/config must describe the request's attribution, tools,
/// tool choice and cache controls: diagnostics still derive those fields from
/// the supplied prompt/config, rather than inspecting arbitrary request JSON.
#[derive(Serialize)]
#[serde(transparent)]
pub struct PreparedSseRequest {
    /// Exact request JSON, including admitted opaque replay syntax.
    body: Box<RawValue>,
    /// Model identity used to reject mismatched attempt diagnostics.
    #[serde(skip)]
    pub(super) model: String,
    /// Actual input cardinality for content-free diagnostics.
    #[serde(skip)]
    pub(super) input_items: usize,
    /// Allowlisted actual reasoning selector for content-free diagnostics.
    #[serde(skip)]
    pub(super) reasoning_selector: Option<&'static str>,
    /// Adapter-selected terminal limits that must never enter automatic retry.
    #[serde(skip)]
    pub(super) non_retryable_incomplete_reasons: &'static [&'static str],
}

/// Content-free failure to construct a supported full-replay SSE request.
#[derive(Debug)]
pub struct PrepareSseRequestError;

impl std::fmt::Display for PrepareSseRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid prepared Responses SSE request")
    }
}

impl std::error::Error for PrepareSseRequestError {}

impl PreparedSseRequest {
    /// Lower the ordinary generic request without changing its wire policy.
    ///
    /// Adapters can read the exact JSON, transform only their owned fields
    /// using raw JSON values, then validate the result with
    /// [`Self::from_json`].
    pub fn lower(
        prompt: &tau_proto::AgentPromptCreated,
        config: &AttemptConfig,
        model: &AttemptModel,
    ) -> Result<Self, PrepareSseRequestError> {
        let body = build_request(prompt, config, model).map_err(|_| PrepareSseRequestError)?;
        let raw = serde_json::value::to_raw_value(&body).map_err(|_| PrepareSseRequestError)?;
        Self::from_json(raw)
    }

    /// Validate the transport envelope without reconstructing its JSON.
    ///
    /// Requires a model, array input and `stream: true`, and rejects any
    /// `previous_response_id` member. This does not validate provider-specific
    /// request fields or grant opaque replay authority.
    pub fn from_json(body: Box<RawValue>) -> Result<Self, PrepareSseRequestError> {
        Self::parse(body).map_err(|_| PrepareSseRequestError)
    }

    /// Borrow the exact final body; no transcript or opaque item is
    /// reserialized.
    pub fn json(&self) -> &RawValue {
        &self.body
    }

    /// Select exact provider-owned incomplete reasons that stop with an Error
    /// while retaining validated partial assistant prose and terminal
    /// accounting.
    ///
    /// These outcomes omit every tool call and opaque item and grant neither
    /// output-length continuation nor context-overflow recovery. Ordinary
    /// generic requests retain their existing terminal policy. This is trusted
    /// adapter policy, never a field sent to the upstream API.
    pub fn with_non_retryable_incomplete_reasons(
        mut self,
        reasons: &'static [&'static str],
    ) -> Self {
        self.non_retryable_incomplete_reasons = reasons;
        self
    }

    /// Extract only neutral, bounded diagnostic selectors from the envelope.
    fn parse(body: Box<RawValue>) -> Result<Self, Error> {
        let fields: BTreeMap<&str, &RawValue> =
            serde_json::from_str(body.get()).map_err(|_| Error::InvalidRequest)?;
        if fields.contains_key("previous_response_id")
            || fields.get("stream").map(|raw| raw.get()) != Some("true")
        {
            return Err(Error::InvalidRequest);
        }
        let model: String =
            serde_json::from_str(fields.get("model").ok_or(Error::InvalidRequest)?.get())
                .map_err(|_| Error::InvalidRequest)?;
        if model.trim().is_empty() {
            return Err(Error::InvalidRequest);
        }
        let input: Vec<serde::de::IgnoredAny> =
            serde_json::from_str(fields.get("input").ok_or(Error::InvalidRequest)?.get())
                .map_err(|_| Error::InvalidRequest)?;
        let reasoning_selector = fields
            .get("reasoning")
            .and_then(|raw| serde_json::from_str::<ReasoningSelector<'_>>(raw.get()).ok())
            .and_then(|reasoning| match reasoning.effort {
                "none" => Some("none"),
                "minimal" => Some("minimal"),
                "low" => Some("low"),
                "medium" => Some("medium"),
                "high" => Some("high"),
                "xhigh" => Some("xhigh"),
                "max" => Some("max"),
                _ => None,
            });
        Ok(Self {
            model,
            input_items: input.len(),
            reasoning_selector,
            non_retryable_incomplete_reasons: &[],
            body,
        })
    }
}

/// Borrow only the requested effort, never retaining arbitrary diagnostic text.
#[derive(Deserialize)]
struct ReasoningSelector<'a> {
    /// Provider-supplied selector, admitted to diagnostics only by allowlist.
    effort: &'a str,
}
