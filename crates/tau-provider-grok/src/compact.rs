//! One-shot xAI opaque compaction, independent of streaming inference.

use std::time::Duration;

use serde::Deserialize;
use serde_json::value::RawValue;
use tau_proto::{ContextItem, OpaqueProviderItem, ProviderTokenUsage, ValidatedCompactionWindow};
use tokio::runtime::Builder;
use tokio::time::Instant;

const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const DEADLINE: Duration = Duration::from_secs(300);

/// Exact successful provider replacement and charged token counts.
pub struct Success {
    /// The sole validated, raw-preserving provider item.
    pub output: ContextItem,
    /// Provider-reported input, cached input, and output token counts.
    pub usage: Option<ProviderTokenUsage>,
    /// Provider's compaction response identifier.
    pub response_id: Option<String>,
}

/// One non-retryable native compaction attempt.
pub enum Outcome {
    /// Validated output ready for the harness transaction.
    Completed(Box<Success>),
    /// The caller canceled before publication.
    Canceled,
    /// A redacted failure; `dispatched` records whether paid work may have
    /// begun.
    Failed { dispatched: bool },
}

/// Submit once with no implicit HTTP retry and a bounded, cancelable response.
///
/// Ambiguous failures never fall back to another paid summary request.
pub fn run(
    body: &RawValue,
    base_url: &str,
    bearer: &str,
    network: &tau_provider::OutboundNetworkPolicy,
    is_canceled: &mut impl FnMut() -> bool,
    diagnostics: &tau_provider_responses::CompactDiagnostics<'_>,
) -> Outcome {
    if is_canceled() {
        diagnostics.finish(false, true);
        return Outcome::Canceled;
    }
    let Ok(runtime) = Builder::new_current_thread().enable_all().build() else {
        diagnostics.failed("runtime_failure");
        diagnostics.finish(false, false);
        return Outcome::Failed { dispatched: false };
    };
    let mut dispatched = false;
    let mut response_observed = false;
    let url = format!("{}/responses/compact", base_url.trim_end_matches('/'));
    let result = runtime.block_on(async {
        let client = network.client_for_without_retries(&url).map_err(|_| ())?;
        let request = client
            .post(&url)
            .header("content-type", "application/json")
            .bearer_auth(bearer)
            .body(body.get().to_owned());
        diagnostics.request(body);
        dispatched = true;
        diagnostics.dispatch(body);
        let deadline = Instant::now() + DEADLINE;
        let mut response = wait(request.send(), deadline, is_canceled)
            .await?
            .map_err(|_| ())?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let mut bytes = Vec::new();
            while bytes.len() < MAX_ERROR_BODY_BYTES {
                let chunk = wait(response.chunk(), deadline, is_canceled)
                    .await?
                    .map_err(|_| ())?;
                let Some(chunk) = chunk else { break };
                if chunk.len() > MAX_ERROR_BODY_BYTES.saturating_sub(bytes.len()) {
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            diagnostics.rejected(status, &bytes);
            response_observed = true;
            return Err(());
        }
        let mut bytes = Vec::new();
        loop {
            let chunk = wait(response.chunk(), deadline, is_canceled)
                .await?
                .map_err(|_| ())?;
            let Some(chunk) = chunk else { break };
            if chunk.len() > MAX_BODY_BYTES.saturating_sub(bytes.len()) {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        let parsed = parse(&bytes);
        diagnostics.response(&bytes, parsed.is_ok());
        response_observed = true;
        parsed
    });
    let outcome = classify(result, is_canceled(), dispatched);
    if matches!(outcome, Outcome::Failed { .. }) && !response_observed {
        diagnostics.failed("transport_or_timeout");
    }
    diagnostics.finish(
        matches!(outcome, Outcome::Completed(_)),
        matches!(outcome, Outcome::Canceled),
    );
    outcome
}

/// Give cancellation precedence over completed or ambiguous provider work.
fn classify(result: Result<Success, ()>, canceled: bool, dispatched: bool) -> Outcome {
    if canceled {
        return Outcome::Canceled;
    }
    match result {
        Ok(success) => Outcome::Completed(Box::new(success)),
        Err(()) => Outcome::Failed { dispatched },
    }
}

/// Poll the caller's cancellation authority while bounding all HTTP phases.
async fn wait<T>(
    future: impl std::future::Future<Output = T>,
    deadline: Instant,
    is_canceled: &mut impl FnMut() -> bool,
) -> Result<T, ()> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return Ok(result),
            () = tokio::time::sleep_until(deadline) => return Err(()),
            () = tokio::time::sleep(Duration::from_millis(250)) => {
                if is_canceled() { return Err(()); }
            }
        }
    }
}

/// Reject all output shapes except one complete xAI opaque compaction item.
fn parse(bytes: &[u8]) -> Result<Success, ()> {
    #[derive(Deserialize)]
    /// The documented xAI finite compact envelope.
    struct Response {
        /// Endpoint response family.
        object: String,
        /// Identity echoed by the sole output item.
        id: String,
        /// Raw provider items, preserving exact replay syntax.
        output: Vec<Box<RawValue>>,
        /// Optional provider-reported charged tokens.
        usage: Option<serde_json::Value>,
    }
    let response: Response = serde_json::from_slice(bytes).map_err(|_| ())?;
    if response.object != "response.compaction"
        || response.id.is_empty()
        || response.output.len() != 1
    {
        return Err(());
    }
    let raw = response.output.into_iter().next().ok_or(())?;
    let fields: serde_json::Value = serde_json::from_str(raw.get()).map_err(|_| ())?;
    if fields["type"] != "compaction"
        || fields["id"].as_str() != Some(response.id.as_str())
        || fields["encrypted_content"]
            .as_str()
            .is_none_or(str::is_empty)
    {
        return Err(());
    }
    let item =
        ContextItem::Compaction(OpaqueProviderItem::from_raw_json(raw.get()).map_err(|_| ())?);
    let mut output = ValidatedCompactionWindow::new(vec![item])
        .map_err(|_| ())?
        .into_items();
    let usage = response.usage.and_then(|usage| {
        let input = usage["input_tokens"].as_u64()?;
        let output = usage["output_tokens"].as_u64()?;
        Some(ProviderTokenUsage {
            model: None,
            prompt_sent_tokens: input,
            prompt_cached_tokens: usage["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0),
            prompt_cache_read_ceiling_tokens: None,
            cache: None,
            response_received_tokens: output,
            stats: Default::default(),
        })
    });
    Ok(Success {
        output: output.pop().ok_or(())?,
        usage,
        response_id: Some(response.id),
    })
}

#[cfg(test)]
mod tests;
