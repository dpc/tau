//! Optional private evidence for provider-owned unary Responses compaction.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;
use tau_proto::{AgentPromptCreated, ProviderAttempt};
use tau_provider::cache_diagnostic::CacheDiagnostics;

use crate::cache_diagnostic::CacheAttempt;
use crate::debug_capture::DebugCapture;
use crate::{AttemptConfig, AttemptModel};

/// Borrowed attempt attribution with the shared bounded private capture and
/// scalar diagnostic policy. Neither sink has canonical outcome authority.
pub struct CompactDiagnostics<'a> {
    /// Durable prompt attribution and context cardinality.
    prompt: &'a AgentPromptCreated,
    /// Effective route and private credential for capture sanitization.
    config: AttemptConfig,
    /// Exact selected model identity.
    model: AttemptModel,
    /// Existing sanitized best-effort exact-capture sink.
    capture: DebugCapture,
}

impl<'a> CompactDiagnostics<'a> {
    /// Select private diagnostics using the same independently controlled
    /// exact/metadata policies as an ordinary public Responses attempt.
    pub fn new(
        prompt: &'a AgentPromptCreated,
        model: AttemptModel,
        base_url: String,
        api_key: String,
        debug_provider_requests: bool,
        cache_diagnostics: CacheDiagnostics,
        provider_attempt: ProviderAttempt,
    ) -> Self {
        let config = AttemptConfig {
            base_url,
            api_key,
            max_output_tokens: 0,
            transport: crate::Transport::Sse,
            prompt_cache: None,
        };
        let mut capture = DebugCapture::new(debug_provider_requests);
        capture.cache = CacheAttempt::new(
            prompt,
            debug_provider_requests,
            cache_diagnostics,
            Some(provider_attempt),
        )
        .map(Arc::new);
        Self {
            prompt,
            config,
            model,
            capture,
        }
    }

    /// Capture only the final compact body before the single HTTP dispatch.
    pub fn request(&self, body: &RawValue) {
        self.capture
            .submit_unary_request(self.prompt, &self.config, &self.model, body);
    }

    /// Publish an allowlisted scalar dispatch observation, independent of the
    /// exact private request-capture setting.
    pub fn dispatch(&self, body: &RawValue) {
        if let Some(cache) = &self.capture.cache {
            #[derive(Deserialize)]
            struct InputCount {
                /// Number of actual Responses items after trigger removal.
                input: Vec<serde::de::IgnoredAny>,
            }
            let input_items =
                serde_json::from_str::<InputCount>(body.get()).map_or(0, |value| value.input.len());
            cache.dispatch_compact(
                self.prompt,
                &self.config,
                &self.model,
                input_items,
                body.get().len(),
            );
        }
    }

    /// Capture a bounded raw successful or malformed unary response and extract
    /// only scalar reported usage for independent cache diagnostics.
    pub fn response(&self, body: &[u8], valid: bool) {
        if body.len() <= 512 * 1024
            && let Some(cache) = &self.capture.cache
            && let Ok(value) = serde_json::from_slice::<Value>(body)
        {
            cache.record_usage(value.get("usage"));
        }
        self.capture.submit_unary_response(
            self.prompt,
            &self.config,
            &self.model,
            Some(200),
            if valid {
                "complete"
            } else {
                "invalid_response"
            },
            std::str::from_utf8(body).ok(),
        );
    }

    /// Capture the closed HTTP status and bounded provider error body only in
    /// the private sink; terminal reports retain no provider prose.
    pub fn rejected(&self, status: u16, body: &[u8]) {
        self.capture.submit_unary_response(
            self.prompt,
            &self.config,
            &self.model,
            Some(status),
            "http_rejected",
            std::str::from_utf8(body).ok(),
        );
    }

    /// Capture a content-free transport, timeout, or body limit failure.
    pub fn failed(&self, kind: &'static str) {
        self.capture.submit_unary_response(
            self.prompt,
            &self.config,
            &self.model,
            None,
            kind,
            None,
        );
    }

    /// Close scalar attempt evidence without authorizing another paid request.
    pub fn finish(&self, successful: bool, canceled: bool) {
        if let Some(cache) = &self.capture.cache {
            cache.finish_compact(successful, canceled);
        }
    }
}
