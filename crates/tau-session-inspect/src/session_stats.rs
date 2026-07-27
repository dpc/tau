//! Exact forward-only activity accounting from session and agent journals.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;
use tau_core::{AgentStore, SessionStore};
use tau_proto::{AgentOuterTurnDisposition, AgentOuterTurnId, Effort, Event, ModelId};

use crate::InspectError;

mod activity_counts;
mod agent_activity_stats;
mod missing_accounting_data;
mod model_effort_stats;
mod tool_activity_stats;

pub use activity_counts::ActivityCounts;
pub use agent_activity_stats::AgentActivityStats;
pub use missing_accounting_data::{MissingAccountingData, MissingAccountingFact};
pub use model_effort_stats::ModelEffortStats;
pub use tool_activity_stats::ToolActivityStats;

/// Serialized activity report for one persisted session.
#[derive(Clone, Debug, Serialize)]
pub struct SessionStats {
    /// Stable output schema revision.
    pub schema_version: u32,
    /// Requested session identity.
    pub session_id: tau_proto::SessionId,
    /// Whether every encountered accounting occurrence had the new durable
    /// facts.
    pub complete: bool,
    /// Explicit reasons why historical data could not be accounted exactly.
    pub missing_data: Vec<MissingAccountingData>,
    /// Exact sum across the included agents.
    pub totals: ActivityCounts,
    /// Agents that ever appeared in the session membership journal.
    pub agents: Vec<AgentActivityStats>,
}

/// Captured dispatch authority joined to a later canonical response.
#[derive(Clone)]
struct PromptAccounting {
    /// Captured provider-qualified model.
    model: ModelId,
    /// Captured effort, absent only in legacy records.
    effort: Option<Effort>,
}

/// Traverse one session membership journal and only its members' agent
/// journals.
pub fn read_session_stats(
    sessions_dir: &Path,
    session_id: &tau_proto::SessionId,
) -> Result<Option<SessionStats>, InspectError> {
    if !sessions_dir.try_exists()? {
        return Ok(None);
    }
    let session_store = SessionStore::open_lazy(sessions_dir)?;
    let membership = session_store.session_events(session_id.as_str())?;
    if membership.is_empty()
        && !sessions_dir
            .join(session_id.as_str())
            .join("events.cbor")
            .try_exists()?
    {
        return Ok(None);
    }
    let member_ids = membership
        .iter()
        .filter_map(|record| match &record.event {
            Event::SessionAgentLoaded(loaded) if &loaded.session_id == session_id => {
                Some(loaded.agent_id.clone())
            }
            Event::SessionAgentUnloaded(unloaded) if &unloaded.session_id == session_id => {
                Some(unloaded.agent_id.clone())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let agents_dir = sessions_dir
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("agents");
    let agent_store = agents_dir
        .try_exists()?
        .then(|| AgentStore::open_lazy(&agents_dir))
        .transpose()?;
    let mut agents = Vec::with_capacity(member_ids.len());
    let mut missing = BTreeSet::new();
    let mut totals = ActivityCounts::default();
    for agent_id in member_ids {
        if !agents_dir
            .join(agent_id.as_str())
            .join("events.cbor")
            .try_exists()?
        {
            missing.insert(MissingAccountingData {
                agent_id: agent_id.clone(),
                fact: MissingAccountingFact::AgentJournalMissing,
            });
            agents.push(empty_agent(agent_id.clone()));
            continue;
        }
        let events = agent_store
            .as_ref()
            .expect("existing journal has an existing agents directory")
            .agent_events(agent_id.as_str())?;
        let agent = aggregate_agent(session_id, &agent_id, &events, &mut missing);
        totals.add(&agent.totals);
        agents.push(agent);
    }
    Ok(Some(SessionStats {
        schema_version: 2,
        session_id: session_id
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        complete: missing.is_empty(),
        missing_data: missing.into_iter().collect(),
        totals,
        agents,
    }))
}

fn aggregate_agent(
    session_id: &str,
    agent_id: &tau_proto::AgentId,
    events: &[tau_core::PersistedAgentEvent],
    missing: &mut BTreeSet<MissingAccountingData>,
) -> AgentActivityStats {
    let mut result = empty_agent(agent_id.clone());
    let mut prompts = HashMap::new();
    let mut models: BTreeMap<(ModelId, Effort), ActivityCounts> = BTreeMap::new();
    let mut tools: BTreeMap<String, ToolActivityStats> = BTreeMap::new();
    let mut open_turns = HashSet::<AgentOuterTurnId>::new();
    let mut selected_calls = HashMap::new();
    for record in events {
        match &record.event {
            Event::AgentStarted(started) => {
                result.role = Some(started.role.clone());
                result.name = started.display_name.clone();
                result.creator.clone_from(&started.creator);
                if started.creator.is_none() {
                    note_missing(
                        missing,
                        agent_id,
                        MissingAccountingFact::AgentStartedCreator,
                    );
                }
            }
            Event::AgentDisplayNameSet(name) => result.name = Some(name.display_name.clone()),
            Event::AgentOuterTurnStarted(turn) if turn.session_id == session_id => {
                result.totals.outer_turns_started =
                    result.totals.outer_turns_started.saturating_add(1);
                open_turns.insert(turn.outer_turn_id.clone());
            }
            Event::AgentOuterTurnFinished(turn) if turn.session_id == session_id => {
                let AgentOuterTurnDisposition::Settled = turn.disposition;
                result.totals.outer_turns_finished =
                    result.totals.outer_turns_finished.saturating_add(1);
                open_turns.remove(&turn.outer_turn_id);
            }
            Event::AgentPromptStarted(prompt) if prompt.session_id == session_id => {
                if prompt.operation.is_inference() {
                    result.totals.inner_turns = result.totals.inner_turns.saturating_add(1);
                }
                if prompt.operation.is_inference() && prompt.outer_turn_id.is_none() {
                    note_missing(missing, agent_id, MissingAccountingFact::PromptOuterTurnId);
                }
                let effort = prompt.model_params.map(|params| params.effort);
                if effort.is_none() {
                    note_missing(missing, agent_id, MissingAccountingFact::PromptModelParams);
                }
                prompts.insert(
                    prompt.agent_prompt_id.clone(),
                    PromptAccounting {
                        model: prompt.model.clone(),
                        effort,
                    },
                );
                if prompt.operation.is_inference()
                    && let Some(effort) = effort
                {
                    models
                        .entry((prompt.model.clone(), effort))
                        .or_default()
                        .inner_turns = models
                        .get(&(prompt.model.clone(), effort))
                        .map_or(1, |counts| counts.inner_turns.saturating_add(1));
                }
            }
            Event::ProviderResponseFinished(response) => {
                let prompt = prompts.get(&response.agent_prompt_id);
                for item in &response.output_items {
                    if let tau_proto::ContextItem::ToolCall(call) = item {
                        selected_calls.insert(call.call_id.clone(), prompt.is_some());
                    }
                }
                let Some(prompt) = prompt else {
                    continue;
                };
                let bucket = prompt
                    .effort
                    .map(|effort| models.entry((prompt.model.clone(), effort)).or_default());
                account_response(response, &mut result.totals, bucket, &mut tools);
                if response.estimated_api_cost_rates.is_none()
                    || response.estimated_api_cost_increment.is_none()
                {
                    note_missing(
                        missing,
                        agent_id,
                        MissingAccountingFact::ResponseEstimatedCost,
                    );
                }
            }
            Event::ProviderToolResult(terminal) => {
                if selected_calls.remove(&terminal.call_id) != Some(true) {
                    continue;
                }
                tools
                    .entry(terminal.tool_name.to_string())
                    .or_insert_with(|| ToolActivityStats::new(terminal.tool_name.clone()))
                    .results += 1;
                result.totals.tool_results = result.totals.tool_results.saturating_add(1);
            }
            Event::ProviderToolError(terminal) => {
                if selected_calls.remove(&terminal.call_id) != Some(true) {
                    continue;
                }
                tools
                    .entry(terminal.tool_name.to_string())
                    .or_insert_with(|| ToolActivityStats::new(terminal.tool_name.clone()))
                    .errors += 1;
                result.totals.tool_errors = result.totals.tool_errors.saturating_add(1);
            }
            Event::ToolCancelled(terminal) => {
                if selected_calls.remove(&terminal.call_id) != Some(true) {
                    continue;
                }
                tools
                    .entry(terminal.tool_name.to_string())
                    .or_insert_with(|| ToolActivityStats::new(terminal.tool_name.clone()))
                    .cancellations += 1;
                result.totals.tool_cancellations =
                    result.totals.tool_cancellations.saturating_add(1);
            }
            _ => {}
        }
    }
    result.totals.outer_turns_unterminated = u64::try_from(open_turns.len()).unwrap_or(u64::MAX);
    result.models = models
        .into_iter()
        .map(|((model, effort), totals)| ModelEffortStats {
            model,
            effort,
            totals,
        })
        .collect();
    result.tools = tools.into_values().collect();
    result
}

fn account_response(
    response: &tau_proto::ProviderResponseFinished,
    totals: &mut ActivityCounts,
    bucket: Option<&mut ActivityCounts>,
    tools: &mut BTreeMap<String, ToolActivityStats>,
) {
    let mut delta = ActivityCounts::default();
    if let Some(usage) = &response.usage {
        delta.cached_input_tokens = usage.prompt_cached_tokens.min(usage.prompt_sent_tokens);
        delta.uncached_input_tokens = usage
            .prompt_sent_tokens
            .saturating_sub(delta.cached_input_tokens);
        delta.output_tokens = usage.response_received_tokens;
    }
    if let Some(cost) = response.estimated_api_cost_increment {
        delta.estimated_api_cost = cost;
    }
    for item in &response.output_items {
        if let tau_proto::ContextItem::ToolCall(call) = item {
            delta.tool_calls = delta.tool_calls.saturating_add(1);
            let stats = tools
                .entry(call.name.to_string())
                .or_insert_with(|| ToolActivityStats::new(call.name.clone()));
            stats.calls = stats.calls.saturating_add(1);
        }
    }
    totals.add(&delta);
    if let Some(bucket) = bucket {
        bucket.add(&delta);
    }
}

fn note_missing(
    missing: &mut BTreeSet<MissingAccountingData>,
    agent_id: &tau_proto::AgentId,
    fact: MissingAccountingFact,
) {
    missing.insert(MissingAccountingData {
        agent_id: agent_id.clone(),
        fact,
    });
}

fn empty_agent(agent_id: tau_proto::AgentId) -> AgentActivityStats {
    AgentActivityStats {
        agent_id,
        role: None,
        name: None,
        creator: None,
        totals: ActivityCounts::default(),
        models: Vec::new(),
        tools: Vec::new(),
    }
}

#[cfg(test)]
#[path = "session_stats/tests.rs"]
mod tests;
