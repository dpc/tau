//! Composite provider scheduling, attempt normalization, and cancellation
//! arbitration.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tau_proto::{
    CborValue, Event, ToolProgress, ToolResult, ToolStarted, ToolUseState, ToolUseStatus,
};

use super::hosted::{HostedClient, HostedRequest};
use super::{
    AGGREGATE_ERROR_MAX_BYTES, ATTEMPT_CHIP_MAX_CHARS, MAX_PROVIDER_ATTEMPTS,
    MODEL_VISIBLE_FETCH_TOOL_NAME, MODEL_VISIBLE_SEARCH_TOOL_NAME, PARALLEL_REMOTE_FETCH_TOOL,
    PARALLEL_REMOTE_SEARCH_TOOL, ParallelClient, Searcher, TRUNCATED_SUFFIX, WebAdapter,
    WebOperation, cbor_text_field, exa_ok_display, ok_display, parse_exa_args, project_web_content,
    tool_error,
};

/// Validated non-empty ordered provider membership and its next-primary cursor.
pub(super) struct ProviderPool {
    /// Ordered providers; construction rejects empty and duplicate membership.
    providers: Box<[WebAdapter]>,
    /// Index of the next admitted call's primary provider.
    cursor: usize,
}

impl ProviderPool {
    /// Return the configured number of providers.
    pub(super) fn len(&self) -> usize {
        self.providers.len()
    }

    /// Validate one configured operation pool and reset its cursor to index
    /// zero.
    pub(super) fn new(name: &str, providers: Vec<WebAdapter>) -> Result<Self, String> {
        if providers.is_empty() {
            return Err(format!("`{name}` must contain at least one provider"));
        }
        for (index, provider) in providers.iter().enumerate() {
            if providers[..index].contains(provider) {
                return Err(format!(
                    "`{name}` contains duplicate provider `{}`",
                    provider.as_str()
                ));
            }
        }
        Ok(Self {
            providers: providers.into_boxed_slice(),
            cursor: 0,
        })
    }

    /// Reserve one circular, bounded attempt order and advance exactly once.
    pub(super) fn reserve(&mut self) -> Box<[WebAdapter]> {
        let start = self.cursor;
        self.cursor = (start + 1) % self.providers.len();
        (0..self.providers.len().min(MAX_PROVIDER_ATTEMPTS))
            .map(|offset| self.providers[(start + offset) % self.providers.len()])
            .collect()
    }

    /// Return whether the configured pool contains `provider`.
    pub(super) fn contains(&self, provider: WebAdapter) -> bool {
        self.providers.contains(&provider)
    }
}

/// Replace an uncommitted queued result/error when serial cancellation won.
pub(super) fn arbitrate_cancelled_terminal(
    cancellations: &Mutex<HashMap<tau_proto::ToolCallId, Arc<AtomicBool>>>,
    terminal: tau_client::ToolTerminalOutcome,
) -> tau_client::ToolTerminalOutcome {
    let call_id = match &terminal {
        tau_client::ToolTerminalOutcome::Result(result) => &result.call_id,
        tau_client::ToolTerminalOutcome::Failure(error) => &error.call_id,
        tau_client::ToolTerminalOutcome::Cancelled(cancelled) => &cancelled.call_id,
    };
    let cancellation_won = cancellations
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(call_id)
        .is_some_and(|cancelled| cancelled.load(Ordering::Acquire));
    if !cancellation_won {
        return terminal;
    }
    match terminal {
        tau_client::ToolTerminalOutcome::Cancelled(cancelled) => cancelled.into(),
        tau_client::ToolTerminalOutcome::Result(result) => tau_proto::ToolCancelled {
            call_id: result.call_id,
            tool_name: result.tool_name,
            tool_type: result.tool_type,
            presentation: Default::default(),
            display: result.display.map(cancellation_display),
        }
        .into(),
        tau_client::ToolTerminalOutcome::Failure(error) => tau_proto::ToolCancelled {
            call_id: error.call_id,
            tool_name: error.tool_name,
            tool_type: error.tool_type,
            presentation: Default::default(),
            display: error.display.map(cancellation_display),
        }
        .into(),
    }
}

fn cancellation_display(mut display: ToolUseState) -> ToolUseState {
    display.status = ToolUseStatus::Warning;
    display.status_text = "cancelled".to_owned();
    for chip in &mut display.info_chips {
        if let Some(index) = chip.rfind("✓ ") {
            chip.replace_range(index..index + "✓".len(), "⊘");
        }
    }
    display
}

/// Stable provider-originated failure categories exposed in bounded summaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FailureCategory {
    /// Provider rate-limit response.
    RateLimited,
    /// Transport, DNS, TLS, or connection failure.
    Transport,
    /// Provider rejected the canonical request.
    Rejected,
    /// Provider reported an otherwise classified failure.
    Provider,
    /// Provider response did not satisfy the MCP result contract.
    InvalidResponse,
    /// Provider response or projected content exceeded a bound.
    Oversize,
}

impl FailureCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::Transport => "transport",
            Self::Rejected => "rejected",
            Self::Provider => "provider",
            Self::InvalidResponse => "invalid_response",
            Self::Oversize => "oversize",
        }
    }
}

/// Exhaustive outcome of one issued provider attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AttemptOutcome {
    /// Provider returned usable non-empty text.
    Success,
    /// Provider returned successful but trimmed-empty text.
    Empty,
    /// Attempt exhausted its scheduler-owned deadline slice.
    Deadline,
    /// Cancellation won while this provider request was issued.
    Cancelled,
    /// Provider attempt failed in one stable category.
    Failure(FailureCategory),
}

impl AttemptOutcome {
    const fn marker(self) -> &'static str {
        match self {
            Self::Success => "✓",
            Self::Empty => "∅",
            Self::Deadline => "⏱",
            Self::Cancelled => "⊘",
            Self::Failure(_) => "✗",
        }
    }

    const fn category(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Empty => "empty",
            Self::Deadline => "deadline",
            Self::Cancelled => "cancelled",
            Self::Failure(category) => category.as_str(),
        }
    }
}

/// One issued provider attempt and its exhaustive normalized outcome.
pub(super) struct AttemptRecord {
    /// Provider contacted for this attempt.
    pub(super) provider: WebAdapter,
    /// Exact normalized attempt outcome.
    pub(super) outcome: AttemptOutcome,
}

/// Provider-integration seam consumed by the provider-neutral scheduler.
pub(super) trait ProviderDispatcher {
    /// Issue one operation-specific provider attempt within the supplied slice.
    fn call(
        &self,
        provider: WebAdapter,
        operation: WebOperation,
        search: Option<(&str, u32)>,
        url: Option<&str>,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<String, String>;
}

/// Production adapter registry for the currently integrated hosted providers.
pub(super) struct HostedProviderDispatcher<'a> {
    /// Exa search/fetch implementation.
    pub(super) searcher: &'a dyn Searcher,
    /// Parallel search/fetch implementation.
    pub(super) parallel_client: &'a dyn ParallelClient,
    /// Additional hosted provider implementations.
    pub(super) hosted_client: &'a dyn HostedClient,
}

impl ProviderDispatcher for HostedProviderDispatcher<'_> {
    fn call(
        &self,
        provider: WebAdapter,
        operation: WebOperation,
        search: Option<(&str, u32)>,
        url: Option<&str>,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<String, String> {
        match (provider, operation) {
            (WebAdapter::Exa, WebOperation::Search) => {
                let (query, num_results) = search.expect("validated search args");
                self.searcher
                    .search_with_timeout(query, num_results, timeout)
            }
            (WebAdapter::Exa, WebOperation::Fetch) => self
                .searcher
                .fetch_with_timeout(url.expect("validated fetch URL"), timeout),
            (WebAdapter::Parallel, WebOperation::Search) => {
                let (query, _) = search.expect("validated search args");
                self.parallel_client.call_with_timeout(
                    PARALLEL_REMOTE_SEARCH_TOOL,
                    serde_json::json!({"query": query}),
                    timeout,
                )
            }
            (WebAdapter::Parallel, WebOperation::Fetch) => self.parallel_client.call_with_timeout(
                PARALLEL_REMOTE_FETCH_TOOL,
                serde_json::json!({"urls": [url.expect("validated fetch URL")]}),
                timeout,
            ),
            (
                provider @ (WebAdapter::You
                | WebAdapter::Brave
                | WebAdapter::Tavily
                | WebAdapter::Firecrawl),
                operation,
            ) => {
                let (query, count) = search.unwrap_or(("", 0));
                self.hosted_client.call(
                    provider,
                    HostedRequest {
                        operation,
                        query,
                        count,
                        url: url.unwrap_or(""),
                        timeout,
                        cancelled,
                    },
                )
            }
            #[cfg(test)]
            (WebAdapter::Third | WebAdapter::Fourth, _) => {
                Err("test provider requires an injected dispatcher".to_owned())
            }
        }
    }
}

/// One admitted composite call with its immutable scheduling and provider
/// state.
pub(super) struct CompositeCall<'a> {
    /// Routed tool invocation being executed.
    pub(super) invoke: ToolStarted,
    /// Provider capability selected by the routed tool.
    pub(super) operation: WebOperation,
    /// Reserved circular provider order, bounded during admission.
    pub(super) providers: Box<[WebAdapter]>,
    /// Safe query or fetch-host display arguments.
    pub(super) display_args: String,
    /// Cooperative cancellation flag owned by the serial protocol loop.
    pub(super) cancelled: &'a AtomicBool,
    /// Provider adapter registry used for every reserved provider identity.
    pub(super) dispatcher: &'a dyn ProviderDispatcher,
    /// Progress event sink; absent only in direct scheduler tests.
    pub(super) handle: Option<&'a tau_client::ClientHandle>,
    /// Admission-anchored total deadline shared by all attempts.
    pub(super) deadline: Instant,
}

impl CompositeCall<'_> {
    /// Run the reserved attempts sequentially and produce one terminal event.
    pub(super) fn run(self) -> Event {
        let parsed_search = match self.operation {
            WebOperation::Search => match parse_exa_args(&self.invoke.arguments) {
                Ok(parsed) => Some(parsed),
                Err(message) => return tool_error(self.invoke, message, self.display_args),
            },
            WebOperation::Fetch => None,
        };
        let parsed_url = match self.operation {
            WebOperation::Fetch => match cbor_text_field(&self.invoke.arguments, "url") {
                Some(url) => Some(url),
                None => {
                    return tool_error(
                        self.invoke,
                        "missing string argument: url".to_owned(),
                        self.display_args,
                    );
                }
            },
            WebOperation::Search => None,
        };
        let mut attempts = Vec::new();

        for (index, provider) in self.providers.iter().copied().enumerate() {
            if self.cancelled.load(Ordering::Acquire) {
                return cancelled_event(self.invoke, self.display_args, attempts);
            }
            let Some(remaining) = self.deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            if remaining.is_zero() {
                break;
            }
            let attempts_left = self.providers.len() - index;
            let attempt_timeout = attempt_budget(remaining, attempts_left);
            if let Some(handle) = self.handle {
                report_attempt_progress(
                    handle,
                    &self.invoke,
                    &self.display_args,
                    &attempts,
                    provider,
                );
            }
            let before = Instant::now();
            let search = parsed_search
                .as_ref()
                .map(|(query, count)| (query.as_str(), *count));
            let result = self.dispatcher.call(
                provider,
                self.operation,
                search,
                parsed_url.as_deref(),
                attempt_timeout,
                self.cancelled,
            );
            if self.cancelled.load(Ordering::Acquire) {
                attempts.push(AttemptRecord {
                    provider,
                    outcome: AttemptOutcome::Cancelled,
                });
                return cancelled_event(self.invoke, self.display_args, attempts);
            }
            if attempt_timeout <= before.elapsed() {
                attempts.push(AttemptRecord {
                    provider,
                    outcome: AttemptOutcome::Deadline,
                });
                continue;
            }
            match result {
                Ok(text) if text.trim().is_empty() => attempts.push(AttemptRecord {
                    provider,
                    outcome: AttemptOutcome::Empty,
                }),
                Ok(text) => {
                    attempts.push(AttemptRecord {
                        provider,
                        outcome: AttemptOutcome::Success,
                    });
                    let projected = match project_web_content(provider, self.operation, &text) {
                        Ok(projected) => projected,
                        Err(_) => {
                            attempts.last_mut().expect("current attempt").outcome =
                                AttemptOutcome::Failure(FailureCategory::Oversize);
                            if index + 1 < self.providers.len() {
                                continue;
                            }
                            return aggregate_error(
                                self.invoke,
                                self.operation,
                                self.display_args,
                                attempts,
                            );
                        }
                    };
                    let mut display =
                        if provider == WebAdapter::Exa && self.operation == WebOperation::Search {
                            exa_ok_display(&text, self.display_args)
                        } else {
                            ok_display(&text, self.display_args)
                        };
                    display.info_chips = vec![attempt_chip(&attempts, None)];
                    return Event::ToolResult(ToolResult {
                        presentation: Default::default(),
                        call_id: self.invoke.call_id,
                        tool_name: self.invoke.tool_name,
                        tool_type: tau_proto::ToolType::Function,
                        result: CborValue::Text(projected),
                        provider_content: Vec::new(),
                        kind: tau_proto::ToolResultKind::Final,
                        display: Some(display),
                        originator: self.invoke.originator,
                    });
                }
                Err(message) => attempts.push(AttemptRecord {
                    provider,
                    outcome: classify_provider_error(&message),
                }),
            }
        }
        aggregate_error(self.invoke, self.operation, self.display_args, attempts)
    }
}

/// Divide remaining total deadline time among remaining allowed attempts.
pub(super) fn attempt_budget(remaining: Duration, attempts_left: usize) -> Duration {
    remaining / u32::try_from(attempts_left).unwrap_or(1)
}

fn report_attempt_progress(
    handle: &tau_client::ClientHandle,
    invoke: &ToolStarted,
    display_args: &str,
    attempts: &[AttemptRecord],
    current: WebAdapter,
) {
    let mut display = ToolUseState {
        args: display_args.to_owned(),
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        ..Default::default()
    };
    display.info_chips = vec![attempt_chip(attempts, Some(current))];
    let _ = handle.report_tool_progress_detached(ToolProgress {
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        message: None,
        progress: None,
        display: Some(display),
    });
}

/// Render one bounded, ordered attempt-history information chip.
pub(super) fn attempt_chip(attempts: &[AttemptRecord], current: Option<WebAdapter>) -> String {
    let mut entries = attempts
        .iter()
        .map(|attempt| {
            format!(
                "{} {}",
                attempt.outcome.marker(),
                attempt.provider.display_name()
            )
        })
        .collect::<Vec<_>>();
    if let Some(provider) = current {
        entries.push(format!("… {}", provider.display_name()));
    }
    let full = entries.join(" → ");
    if full.chars().count() <= ATTEMPT_CHIP_MAX_CHARS || entries.len() <= 2 {
        return full;
    }
    format!(
        "{} → … +{} → {}",
        entries[0],
        entries.len() - 2,
        entries.last().expect("at least three attempt entries")
    )
}

pub(super) fn classify_provider_error(message: &str) -> AttemptOutcome {
    let lower = message.to_ascii_lowercase();
    if lower.contains("rate-limit") || lower.contains("429") {
        AttemptOutcome::Failure(FailureCategory::RateLimited)
    } else if lower.contains("timed out") || lower.contains("timeout") {
        AttemptOutcome::Deadline
    } else if lower.contains("transport")
        || lower.contains("dns")
        || lower.contains("tls")
        || lower.contains("connection")
    {
        AttemptOutcome::Failure(FailureCategory::Transport)
    } else if lower.contains("http 4")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
    {
        AttemptOutcome::Failure(FailureCategory::Rejected)
    } else if lower.contains("invalid response")
        || lower.contains("utf-8")
        || lower.contains("invalid json")
        || lower.contains("missing `result.content`")
        || lower.contains("no text content")
        || lower.contains("sse")
    {
        AttemptOutcome::Failure(FailureCategory::InvalidResponse)
    } else if lower.contains("exceeded") {
        AttemptOutcome::Failure(FailureCategory::Oversize)
    } else {
        AttemptOutcome::Failure(FailureCategory::Provider)
    }
}

fn aggregate_error(
    invoke: ToolStarted,
    operation: WebOperation,
    display_args: String,
    attempts: Vec<AttemptRecord>,
) -> Event {
    let operation_name = match operation {
        WebOperation::Search => MODEL_VISIBLE_SEARCH_TOOL_NAME,
        WebOperation::Fetch => MODEL_VISIBLE_FETCH_TOOL_NAME,
    };
    let summary = attempts
        .iter()
        .map(|attempt| {
            format!(
                "{}={}",
                attempt.provider.as_str(),
                attempt.outcome.category()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut message = format!(
        "{operation_name} failed after {} attempts: {summary}",
        attempts.len()
    );
    if AGGREGATE_ERROR_MAX_BYTES < message.len() {
        message = cap_utf8_with_suffix(message, AGGREGATE_ERROR_MAX_BYTES);
    }
    let mut event = tool_error(invoke, message, display_args);
    if let Event::ToolError(error) = &mut event {
        error.details = None;
        if let Some(display) = &mut error.display {
            display.info_chips = vec![attempt_chip(&attempts, None)];
        }
    }
    event
}

fn cancelled_event(
    invoke: ToolStarted,
    display_args: String,
    attempts: Vec<AttemptRecord>,
) -> Event {
    let mut display = ToolUseState {
        args: display_args,
        status: ToolUseStatus::Warning,
        status_text: "cancelled".to_owned(),
        ..Default::default()
    };
    let chip = attempt_chip(&attempts, None);
    if !chip.is_empty() {
        display.info_chips = vec![chip];
    }
    Event::ToolCancelled(tau_proto::ToolCancelled {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        presentation: Default::default(),
        display: Some(display),
    })
}

fn cap_utf8_with_suffix(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes.saturating_sub(TRUNCATED_SUFFIX.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(TRUNCATED_SUFFIX);
    text
}
