/// Accounting supplied by a failed Responses envelope, without completion or
/// continuation authority. Intermediate retries are not durable terminals.
#[derive(Clone, Default)]
pub struct ResponseAccounting {
    /// Response-local token usage, absent when no valid counters were supplied.
    pub usage: Option<tau_proto::ProviderTokenUsage>,
    /// Provider response identity, retained for accounting rather than
    /// chaining.
    pub provider_response_id: Option<String>,
}

impl std::fmt::Debug for ResponseAccounting {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ResponseAccounting(<redacted>)")
    }
}
