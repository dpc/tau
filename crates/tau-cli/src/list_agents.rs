//! Running-session agent roster command and picker projection.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::Duration;
use std::{io, time as path_std_time};

use tau_proto::{
    GetSessionAgentList, HarnessInputMessage, HarnessOutputMessage, SessionAgentFacts,
    SessionAgentLifecycle, SessionAgentListEntry, SessionAgentListScope, SessionAgentPersistence,
};

use crate::CliError;

const AGENT_LIST_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// User-visible roster category filters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AgentListFilter {
    /// Include suspended live rows.
    pub(crate) include_suspended: bool,
    /// Include unavailable lifecycle and missing, invalid, or unreadable facts.
    pub(crate) include_unavailable: bool,
    /// Include historical unloaded rows.
    pub(crate) include_unloaded: bool,
}

impl AgentListFilter {
    fn from_args(args: &crate::cli::AgentListArgs) -> Self {
        if args.all {
            return Self {
                include_suspended: true,
                include_unavailable: true,
                include_unloaded: true,
            };
        }
        Self {
            include_suspended: args.include_suspended,
            include_unavailable: args.include_unavailable,
            include_unloaded: args.include_unloaded,
        }
    }
}

/// Category selected by an attached terminal agent-picker binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentPickerFilter {
    /// Include only agents currently eligible for automatic navigation.
    Active,
    /// Include every live agent regardless of navigation eligibility.
    All,
}

impl AgentPickerFilter {
    /// Return whether one lifecycle snapshot belongs in this picker category.
    pub(crate) fn admits(self, lifecycle: SessionAgentLifecycle) -> bool {
        let SessionAgentLifecycle::Live {
            runtime_state,
            navigation_mode,
        } = lifecycle
        else {
            return false;
        };
        match self {
            Self::Active => {
                crate::agent_navigation::is_navigation_eligible(navigation_mode, runtime_state)
            }
            Self::All => true,
        }
    }
}

/// Runs `tau agent list`.
pub(crate) fn run(args: &crate::cli::AgentListArgs) -> Result<(), CliError> {
    let filter = AgentListFilter::from_args(args);
    let scope = if filter.include_unloaded {
        SessionAgentListScope::History
    } else {
        SessionAgentListScope::Current
    };
    let harness_path = tau_harness::runtime_dir::find_harness_for_session(args.session_id.as_str())
        .map_err(|error| CliError::Participant(error.to_string()))?
        .ok_or_else(|| {
            CliError::Participant(format!(
                "no running harness for session `{}`",
                args.session_id
            ))
        })?;
    let agents = request_at_socket_with_timeout_typed(
        &tau_harness::runtime_dir::socket_path(&harness_path),
        &args.session_id,
        scope,
        AGENT_LIST_RPC_TIMEOUT,
    )?;
    let output = format_rows(&visible_agents(agents, filter));
    crate::line_output::write_stdout(&output)
}

/// Requests one agent roster directly from a running harness socket.
pub(crate) fn request_at_socket(
    socket_path: &Path,
    session_id: &tau_proto::SessionId,
    scope: SessionAgentListScope,
) -> Result<Vec<SessionAgentListEntry>, CliError> {
    request_at_socket_with_timeout_typed(socket_path, session_id, scope, AGENT_LIST_RPC_TIMEOUT)
}

fn request_at_socket_with_timeout_typed(
    socket_path: &Path,
    session_id: &tau_proto::SessionId,
    scope: SessionAgentListScope,
    timeout: Duration,
) -> Result<Vec<SessionAgentListEntry>, CliError> {
    let deadline = path_std_time::Instant::now() + timeout;
    let (mut reader, mut writer) =
        crate::ui_client::connect_ui_client_until(socket_path, "tau-list-agents", deadline)?;
    let request_id = crate::ui_client::next_request_id("agent-list");
    crate::ui_client::send_message(
        &mut writer,
        &HarnessInputMessage::GetSessionAgentList(GetSessionAgentList {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            scope,
        }),
    )?;
    loop {
        if path_std_time::Instant::now() >= deadline {
            return Err(CliError::Participant(
                "agent roster request timed out".to_owned(),
            ));
        }
        let message = match reader.read_message() {
            Ok(Some(message)) => message,
            Ok(None) => {
                return Err(CliError::Participant("daemon disconnected".to_owned()));
            }
            Err(tau_proto::DecodeError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) => return Err(CliError::Io(io::Error::other(error))),
        };
        match message {
            HarnessOutputMessage::SessionAgentListResult(result)
                if result.request_id == request_id =>
            {
                if &result.session_id != session_id {
                    return Err(CliError::Participant(
                        "agent roster response targeted a different session".to_owned(),
                    ));
                }
                match result.result {
                    tau_proto::SessionAgentListResultPayload::Ok { agents } => return Ok(agents),
                    tau_proto::SessionAgentListResultPayload::Error { error } => {
                        return Err(CliError::Participant(format!(
                            "agent roster {:?}: {}",
                            error.kind, error.message
                        )));
                    }
                }
            }
            HarnessOutputMessage::Disconnect(disconnect) => {
                return Err(CliError::Participant(
                    disconnect
                        .reason
                        .unwrap_or_else(|| "daemon disconnected".to_owned()),
                ));
            }
            _ => {}
        }
    }
}

/// Filters and topologically orders roster rows for command or picker display.
pub(crate) fn visible_agents(
    agents: Vec<SessionAgentListEntry>,
    filter: AgentListFilter,
) -> Vec<SessionAgentListEntry> {
    let agents = agents
        .into_iter()
        .filter(|agent| match agent.lifecycle {
            SessionAgentLifecycle::Live { .. } => true,
            SessionAgentLifecycle::Unavailable => filter.include_unavailable,
            SessionAgentLifecycle::Unloaded => filter.include_unloaded,
        })
        .filter(|agent| {
            filter.include_unavailable || matches!(agent.facts, SessionAgentFacts::Available { .. })
        })
        .filter(|agent| {
            filter.include_suspended
                || !matches!(
                    agent.lifecycle,
                    SessionAgentLifecycle::Live {
                        navigation_mode: tau_proto::AgentNavigationMode::Suspended,
                        ..
                    }
                )
        })
        .collect();
    topological_order(agents)
}

/// Filters current live roster rows for an attached terminal picker.
pub(crate) fn picker_agents(
    agents: Vec<SessionAgentListEntry>,
    filter: AgentPickerFilter,
) -> Vec<SessionAgentListEntry> {
    topological_order(
        agents
            .into_iter()
            .filter(|agent| filter.admits(agent.lifecycle))
            .collect(),
    )
}

/// Revalidates one picker selection against a fresh current-session snapshot.
pub(crate) fn picker_selection_is_current(
    agents: &[SessionAgentListEntry],
    selected: &tau_proto::AgentId,
    filter: AgentPickerFilter,
) -> bool {
    agents
        .iter()
        .any(|agent| &agent.agent_id == selected && filter.admits(agent.lifecycle))
}

/// Formats rows as stable, headerless, escaped TSV.
pub(crate) fn format_rows(agents: &[SessionAgentListEntry]) -> String {
    format_rows_with(agents, |_| None)
}

/// Formats picker rows with canonical runtime cost, work status, and activity.
pub(crate) fn format_picker_rows(
    agents: &[SessionAgentListEntry],
    cost_for_agent: impl Fn(&tau_proto::AgentId) -> Option<crate::estimated_cost::AgentCostSnapshot>,
) -> String {
    format_rows_with(agents, |agent| {
        let cost = crate::estimated_cost::format_snapshot(cost_for_agent(&agent.agent_id));
        let (status, title) = agent.work_status.as_ref().map_or_else(
            || (work_status_symbol(None).to_owned(), dash()),
            |status| {
                (
                    work_status_symbol(Some(status.phase())).to_owned(),
                    status
                        .title()
                        .map(tau_proto::visible_escape_metadata)
                        .as_deref()
                        .map(crate::line_output::escape_field)
                        .unwrap_or_else(dash),
                )
            },
        );
        let activity = agent.turn_activity.map(turn_activity_symbol).unwrap_or("-");
        Some(vec![cost, status, title, activity.to_owned()])
    })
}

/// Returns the compact human-facing symbol for one reported work phase.
pub(crate) fn work_status_symbol(phase: Option<tau_proto::AgentWorkStatusPhase>) -> &'static str {
    match phase {
        None
        | Some(tau_proto::AgentWorkStatusPhase::Unreported)
        | Some(tau_proto::AgentWorkStatusPhase::Unknown) => "❓",
        Some(tau_proto::AgentWorkStatusPhase::Working) => "🚀",
        Some(tau_proto::AgentWorkStatusPhase::Done) => "✅",
        Some(tau_proto::AgentWorkStatusPhase::Blocked) => "⛔️",
        Some(tau_proto::AgentWorkStatusPhase::Waiting) => "⏳",
    }
}

/// Returns the compact human-facing symbol for detailed turn activity.
pub(crate) fn turn_activity_symbol(activity: tau_proto::AgentTurnActivity) -> &'static str {
    match activity {
        tau_proto::AgentTurnActivity::Responding => "✨",
        tau_proto::AgentTurnActivity::Manipulating => "🔨",
        tau_proto::AgentTurnActivity::Fetching => "🌐",
        tau_proto::AgentTurnActivity::Waiting => "⏳",
        tau_proto::AgentTurnActivity::TimerScheduled => "🕔",
        tau_proto::AgentTurnActivity::Idle => "💤",
    }
}

fn format_rows_with(
    agents: &[SessionAgentListEntry],
    extra_fields: impl Fn(&SessionAgentListEntry) -> Option<Vec<String>>,
) -> String {
    let mut output = String::new();
    for agent in agents {
        let mut fields = vec![
            agent.agent_id.to_string(),
            lifecycle_name(agent.lifecycle).to_owned(),
            lifecycle_runtime(agent.lifecycle)
                .map(runtime_name)
                .unwrap_or("-")
                .to_owned(),
            matches!(agent.lifecycle, SessionAgentLifecycle::Live { .. })
                .then_some(agent.turn_activity)
                .flatten()
                .map(turn_activity_name)
                .unwrap_or("-")
                .to_owned(),
            lifecycle_navigation(agent.lifecycle)
                .map(navigation_name)
                .unwrap_or("-")
                .to_owned(),
            persistence_name(agent.persistence).to_owned(),
            facts_name(&agent.facts).to_owned(),
            facts_role(&agent.facts)
                .map(crate::line_output::escape_field)
                .unwrap_or_else(dash),
            facts_parent(&agent.facts)
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(dash),
            facts_started_at(&agent.facts)
                .map(|timestamp| timestamp.get().to_string())
                .unwrap_or_else(dash),
            facts_display_name(&agent.facts)
                .map(crate::line_output::escape_field)
                .unwrap_or_else(dash),
        ];
        if let Some(extra_fields) = extra_fields(agent) {
            fields.extend(extra_fields);
        }
        output.push_str(&fields.join("\t"));
        output.push('\n');
    }
    output
}

/// Extracts and validates the stable agent id from one selected TSV row.
pub(crate) fn selected_agent_id(row: &str) -> Result<tau_proto::AgentId, String> {
    if row.contains('\n') || row.contains('\r') {
        return Err("fzf returned more than one agent row".to_owned());
    }
    let id = row.split_once('\t').map_or(row, |(id, _)| id);
    tau_proto::AgentId::parse(id)
        .map_err(|error| format!("fzf returned an invalid agent id: {error}"))
}

fn topological_order(agents: Vec<SessionAgentListEntry>) -> Vec<SessionAgentListEntry> {
    let by_id = agents
        .into_iter()
        .map(|agent| (agent.agent_id.clone(), agent))
        .collect::<BTreeMap<_, _>>();
    let mut indegree = by_id
        .keys()
        .cloned()
        .map(|agent_id| (agent_id, 0usize))
        .collect::<HashMap<_, _>>();
    let mut children = BTreeMap::<tau_proto::AgentId, Vec<tau_proto::AgentId>>::new();
    for agent in by_id.values() {
        let Some(parent) = facts_parent(&agent.facts) else {
            continue;
        };
        if parent == &agent.agent_id || !by_id.contains_key(parent) {
            continue;
        }
        *indegree
            .get_mut(&agent.agent_id)
            .expect("every row has an indegree") += 1;
        children
            .entry(parent.clone())
            .or_default()
            .push(agent.agent_id.clone());
    }
    let mut ready = BTreeSet::new();
    for agent in by_id.values() {
        if indegree[&agent.agent_id] == 0 {
            ready.insert(order_key(agent));
        }
    }
    let mut emitted = BTreeSet::new();
    let mut ordered = Vec::with_capacity(by_id.len());
    while ordered.len() < by_id.len() {
        if ready.is_empty()
            && let Some(agent) = by_id
                .values()
                .filter(|agent| !emitted.contains(&agent.agent_id))
                .min_by_key(|agent| order_key(agent))
        {
            ready.insert(order_key(agent));
        }
        let Some(key) = ready.pop_first() else {
            break;
        };
        let agent_id = key.2;
        if !emitted.insert(agent_id.clone()) {
            continue;
        }
        let agent = by_id.get(&agent_id).expect("ready row exists");
        ordered.push(agent.clone());
        for child in children.get(&agent_id).into_iter().flatten() {
            let degree = indegree.get_mut(child).expect("child row exists");
            *degree = degree.saturating_sub(1);
            if *degree == 0 {
                ready.insert(order_key(by_id.get(child).expect("child row exists")));
            }
        }
    }
    ordered
}

fn order_key(agent: &SessionAgentListEntry) -> (u8, u64, tau_proto::AgentId) {
    facts_started_at(&agent.facts).map_or_else(
        || (1, u64::MAX, agent.agent_id.clone()),
        |timestamp| (0, timestamp.get(), agent.agent_id.clone()),
    )
}

fn lifecycle_name(lifecycle: SessionAgentLifecycle) -> &'static str {
    match lifecycle {
        SessionAgentLifecycle::Live { .. } => "live",
        SessionAgentLifecycle::Unavailable => "unavailable",
        SessionAgentLifecycle::Unloaded => "unloaded",
    }
}

fn lifecycle_runtime(lifecycle: SessionAgentLifecycle) -> Option<tau_proto::AgentRuntimeState> {
    match lifecycle {
        SessionAgentLifecycle::Live { runtime_state, .. } => Some(runtime_state),
        SessionAgentLifecycle::Unavailable | SessionAgentLifecycle::Unloaded => None,
    }
}

fn lifecycle_navigation(
    lifecycle: SessionAgentLifecycle,
) -> Option<tau_proto::AgentNavigationMode> {
    match lifecycle {
        SessionAgentLifecycle::Live {
            navigation_mode, ..
        } => Some(navigation_mode),
        SessionAgentLifecycle::Unavailable | SessionAgentLifecycle::Unloaded => None,
    }
}

fn runtime_name(runtime: tau_proto::AgentRuntimeState) -> &'static str {
    match runtime {
        tau_proto::AgentRuntimeState::Idle => "idle",
        tau_proto::AgentRuntimeState::Running => "running",
    }
}

fn turn_activity_name(activity: tau_proto::AgentTurnActivity) -> &'static str {
    match activity {
        tau_proto::AgentTurnActivity::Responding => "responding",
        tau_proto::AgentTurnActivity::Manipulating => "manipulating",
        tau_proto::AgentTurnActivity::Fetching => "fetching",
        tau_proto::AgentTurnActivity::Waiting => "waiting",
        tau_proto::AgentTurnActivity::TimerScheduled => "timer_scheduled",
        tau_proto::AgentTurnActivity::Idle => "idle",
    }
}

fn navigation_name(navigation: tau_proto::AgentNavigationMode) -> &'static str {
    match navigation {
        tau_proto::AgentNavigationMode::Active => "active",
        tau_proto::AgentNavigationMode::ActiveAuto => "active_auto",
        tau_proto::AgentNavigationMode::Suspended => "suspended",
    }
}

fn persistence_name(persistence: SessionAgentPersistence) -> &'static str {
    match persistence {
        SessionAgentPersistence::Durable => "durable",
        SessionAgentPersistence::Ephemeral => "ephemeral",
    }
}

fn facts_name(facts: &SessionAgentFacts) -> &'static str {
    match facts {
        SessionAgentFacts::Available { .. } => "available",
        SessionAgentFacts::Missing => "missing",
        SessionAgentFacts::Invalid => "invalid",
        SessionAgentFacts::Unreadable => "unreadable",
    }
}

fn facts_started_at(facts: &SessionAgentFacts) -> Option<tau_proto::UnixMicros> {
    match facts {
        SessionAgentFacts::Available { started_at, .. } => *started_at,
        _ => None,
    }
}

fn facts_parent(facts: &SessionAgentFacts) -> Option<&tau_proto::AgentId> {
    match facts {
        SessionAgentFacts::Available { parent_agent, .. } => parent_agent.as_ref(),
        _ => None,
    }
}

fn facts_role(facts: &SessionAgentFacts) -> Option<&str> {
    match facts {
        SessionAgentFacts::Available { role, .. } => Some(role),
        _ => None,
    }
}

fn facts_display_name(facts: &SessionAgentFacts) -> Option<&str> {
    match facts {
        SessionAgentFacts::Available { display_name, .. } => display_name.as_deref(),
        _ => None,
    }
}

fn dash() -> String {
    "-".to_owned()
}

#[cfg(test)]
mod tests;
