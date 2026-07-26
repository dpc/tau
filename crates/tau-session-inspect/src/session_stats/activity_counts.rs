use serde::Serialize;

const PICODOLLARS_PER_MICRODOLLAR: u64 = 1_000_000;
const MICRODOLLARS_PER_DOLLAR: f64 = 1_000_000.0;

/// Exact counts shared by session totals, agents, and model/effort buckets.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ActivityCounts {
    /// Accepted outer-turn starts.
    pub outer_turns_started: u64,
    /// Outer turns with a durable terminal fact.
    pub outer_turns_finished: u64,
    /// Starts lacking a matching finish at the journal cut.
    pub outer_turns_unterminated: u64,
    /// Ordinary provider prompt materializations.
    pub inner_turns: u64,
    /// Tool calls emitted by accepted provider responses.
    pub tool_calls: u64,
    /// Successful canonical tool terminals.
    pub tool_results: u64,
    /// Failed canonical tool terminals.
    pub tool_errors: u64,
    /// Cancelled canonical tool terminals.
    pub tool_cancellations: u64,
    /// Response-local uncached input tokens.
    pub uncached_input_tokens: u64,
    /// Response-local cached input tokens.
    pub cached_input_tokens: u64,
    /// Response-local output tokens.
    pub output_tokens: u64,
    /// Sum of harness-captured response increments, presented as USD rounded to
    /// the nearest microdollar.
    ///
    /// The stored value retains the exact fixed-point picodollar accounting
    /// representation.
    #[serde(
        rename = "estimated_api_cost_dollars",
        serialize_with = "serialize_estimated_api_cost_dollars"
    )]
    pub estimated_api_cost: tau_proto::EstimatedApiCost,
}

fn serialize_estimated_api_cost_dollars<S>(
    cost: &tau_proto::EstimatedApiCost,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let rounded_microdollars = (u128::from(cost.as_picodollars())
        + u128::from(PICODOLLARS_PER_MICRODOLLAR / 2))
        / u128::from(PICODOLLARS_PER_MICRODOLLAR);
    serializer.serialize_f64(rounded_microdollars as f64 / MICRODOLLARS_PER_DOLLAR)
}

impl ActivityCounts {
    pub(super) fn add(&mut self, other: &Self) {
        let Self {
            outer_turns_started,
            outer_turns_finished,
            outer_turns_unterminated,
            inner_turns,
            tool_calls,
            tool_results,
            tool_errors,
            tool_cancellations,
            uncached_input_tokens,
            cached_input_tokens,
            output_tokens,
            estimated_api_cost,
        } = other;
        self.outer_turns_started = self
            .outer_turns_started
            .saturating_add(*outer_turns_started);
        self.outer_turns_finished = self
            .outer_turns_finished
            .saturating_add(*outer_turns_finished);
        self.outer_turns_unterminated = self
            .outer_turns_unterminated
            .saturating_add(*outer_turns_unterminated);
        self.inner_turns = self.inner_turns.saturating_add(*inner_turns);
        self.tool_calls = self.tool_calls.saturating_add(*tool_calls);
        self.tool_results = self.tool_results.saturating_add(*tool_results);
        self.tool_errors = self.tool_errors.saturating_add(*tool_errors);
        self.tool_cancellations = self.tool_cancellations.saturating_add(*tool_cancellations);
        self.uncached_input_tokens = self
            .uncached_input_tokens
            .saturating_add(*uncached_input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(*cached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(*output_tokens);
        self.estimated_api_cost = tau_proto::EstimatedApiCost::from_picodollars(
            self.estimated_api_cost
                .as_picodollars()
                .saturating_add(estimated_api_cost.as_picodollars()),
        );
    }
}
