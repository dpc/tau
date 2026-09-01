use std::num::NonZeroUsize;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngCore;
use rand::rngs::OsRng;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};
use tau_client::{ClientError, ExtensionBuilder, ToolContext};
use tau_proto::{
    CborValue, ToolResult, ToolResultKind, ToolSpec, ToolType, ToolUseState, ToolUseStatus,
};
use tau_swarm_api::{
    BlockerId, BlockerPublication, BlockerRevisionNumber, TaskDescription, TaskId, TaskInfo,
    TaskTitle, Timestamp, UpdateId, UpdatePublication,
};

use crate::runtime::SwarmRuntime;

/// Validated owner-history admission bounds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockerHistoryLimits {
    /// Number of retained blocker records across all owners.
    pub(crate) entries: usize,
    /// Encoded bytes allowed in one owner's complete blocker-history response.
    pub(crate) encoded_bytes: usize,
}

impl Default for BlockerHistoryLimits {
    fn default() -> Self {
        Self {
            entries: 256,
            encoded_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Validated immutable-update outbox admission bounds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct UpdateLimits {
    /// Number of unacknowledged immutable updates.
    pub(crate) entries: usize,
    /// Logical UTF-8 bytes retained by immutable update fields.
    pub(crate) logical_bytes: usize,
}

impl Default for UpdateLimits {
    fn default() -> Self {
        Self {
            entries: 256,
            logical_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Logical tool group shared by Tau Swarm's model-visible tools.
pub const TOOL_GROUP_NAME: &str = "swarm";

/// Public name of Tau Swarm's task metadata tool.
const TASK_INFO_TOOL_NAME: &str = "task_info";
/// Public name of Tau Swarm's immutable status-update tool.
const TASK_UPDATE_TOOL_NAME: &str = "task_update";
/// Public name of Tau Swarm's blocker lifecycle tool.
const TASK_BLOCKER_TOOL_NAME: &str = "task_blocker";

/// One process-memory blocker record exposed by `task_blocker(action="list")`.
#[derive(Clone)]
pub(crate) struct BlockerRecord {
    /// Stable random identifier.
    pub blocker_id: BlockerId,
    /// Fixed publication revision.
    pub revision: BlockerRevisionNumber,
    /// Owning Tau agent.
    pub owner: tau_swarm_api::AgentId,
    /// Human-readable title.
    pub title: String,
    /// Full description.
    pub description: String,
    /// Optional recommended answer.
    pub recommended_answer: Option<String>,
    /// Optional associated task.
    pub task_id: Option<TaskId>,
    /// Exhaustive process-memory lifecycle, including an invisible pending
    /// reservation.
    pub(crate) lifecycle: BlockerLifecycle,
}

impl Serialize for BlockerRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut record = serializer.serialize_struct("BlockerRecord", 11)?;
        record.serialize_field("blocker_id", self.blocker_id.as_str())?;
        record.serialize_field("revision", &self.revision.0)?;
        record.serialize_field("owner", self.owner.as_str())?;
        record.serialize_field("title", &self.title)?;
        record.serialize_field("description", &self.description)?;
        record.serialize_field("recommended_answer", &self.recommended_answer)?;
        record.serialize_field("task_id", &self.task_id.as_ref().map(TaskId::as_str))?;
        match &self.lifecycle {
            BlockerLifecycle::Active { .. } => record.serialize_field("state", "active")?,
            BlockerLifecycle::Answered { answer, kind } => {
                record.serialize_field("state", "answered")?;
                record.serialize_field("answer", answer)?;
                let kind = match kind {
                    tau_swarm_api::BlockerAnswerKind::ApprovedRecommendation => {
                        "approved_recommendation"
                    }
                    tau_swarm_api::BlockerAnswerKind::Custom => "custom",
                };
                record.serialize_field("answer_kind", kind)?;
            }
            BlockerLifecycle::Cancelled { reason } => {
                record.serialize_field("state", "cancelled")?;
                if let Some(reason) = reason {
                    record.serialize_field("reason", reason)?;
                }
            }
        }
        record.end()
    }
}

/// Exhaustive process-memory lifecycle of one blocker.
#[derive(Clone)]
pub(crate) enum BlockerLifecycle {
    /// Awaiting an answer, optionally with bytes reserved by one remote
    /// command.
    Active {
        /// Additional encoded bytes held until the pending answer completes.
        pending_answer_bytes: Option<NonZeroUsize>,
    },
    /// Answer accepted through the canonical Tau prompt loopback.
    Answered {
        /// Accepted answer text.
        answer: String,
        /// Accepted answer classification.
        kind: tau_swarm_api::BlockerAnswerKind,
    },
    /// Cancelled by the owning Tau agent.
    Cancelled {
        /// Optional cancellation explanation.
        reason: Option<String>,
    },
}

impl BlockerLifecycle {
    /// Creates an active lifecycle without a pending answer reservation.
    pub(crate) fn active() -> Self {
        Self::Active {
            pending_answer_bytes: None,
        }
    }

    /// Returns the pending answer reservation, or zero when no answer is
    /// pending.
    pub(crate) fn pending_answer_bytes(&self) -> usize {
        match self {
            Self::Active {
                pending_answer_bytes,
            } => pending_answer_bytes.map_or(0, NonZeroUsize::get),
            Self::Answered { .. } | Self::Cancelled { .. } => 0,
        }
    }

    /// Acquires a nonzero answer reservation on an unreserved active blocker.
    pub(crate) fn reserve(&mut self, bytes: NonZeroUsize) {
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(matches!(
            self,
            Self::Active {
                pending_answer_bytes: None
            }
        ));
        *self = Self::Active {
            pending_answer_bytes: Some(bytes),
        };
    }

    /// Releases a pending answer reservation while retaining the active
    /// blocker.
    pub(crate) fn release_reservation(&mut self) {
        if matches!(self, Self::Active { .. }) {
            *self = Self::active();
        }
    }

    /// Commits an accepted answer and consumes any pending reservation.
    pub(crate) fn answer(&mut self, answer: String, kind: tau_swarm_api::BlockerAnswerKind) {
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(matches!(self, Self::Active { .. }));
        *self = Self::Answered { answer, kind };
    }
}

/// Strict tagged operation accepted by the agent-scoped `task_blocker` tool.
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum BlockerArgs {
    /// Opens a blocker owned by the invoking agent.
    Add {
        /// Short human-readable title.
        title: String,
        /// Full blocker description.
        description: String,
        /// Optional proposed answer.
        #[serde(default)]
        recommended_answer: Option<String>,
        /// Optional associated task.
        #[serde(default)]
        task_id: Option<String>,
    },
    /// Cancels one active blocker owned by the invoking agent.
    Cancel {
        /// Stable blocker identifier.
        blocker_id: String,
        /// Optional cancellation explanation.
        #[serde(default)]
        reason: Option<String>,
    },
    /// Lists the invoking agent's complete retained blocker history.
    List {},
}

/// Strict immutable payload accepted by `task_update`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateArgs {
    /// Short human-readable update title.
    title: String,
    /// Complete update description.
    description: String,
    /// Optional related task identifier.
    #[serde(default)]
    task_id: Option<String>,
}

/// Strict replacement payload accepted by `task_info`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskInfoArgs {
    /// Exact opaque task identity.
    task_id: String,
    /// Human-readable title, canonicalized by trimming Unicode whitespace.
    title: String,
    /// Optional description; missing and null both clear the prior description.
    #[serde(default)]
    description: Option<String>,
}

/// Registers Tau Swarm's agent-scoped task tools.
pub(crate) fn register(builder: &mut ExtensionBuilder<SwarmRuntime>) {
    builder
        .scoped_tool(
            tau_proto::ToolName::new(TASK_INFO_TOOL_NAME),
            |_| Ok(declaration(task_info_spec())),
            handle_task_info,
        )
        .scoped_tool(
            tau_proto::ToolName::new(TASK_BLOCKER_TOOL_NAME),
            |_| Ok(declaration(blocker_spec())),
            handle_blocker,
        )
        .scoped_tool(
            tau_proto::ToolName::new(TASK_UPDATE_TOOL_NAME),
            |_| Ok(declaration(update_spec())),
            handle_update,
        );
}

fn task_info_spec() -> ToolSpec {
    common_spec(
        TASK_INFO_TOOL_NAME,
        "Replace the current process-memory title and optional description for a Tau Swarm task.",
        serde_json::json!({
            "type":"object",
            "properties":{
                "task_id":{"type":"string","maxLength":128},
                "title":{"type":"string","maxLength":160},
                "description":{"type":["string","null"],"maxLength":16384}
            },
            "required":["task_id","title"],
            "additionalProperties":false
        }),
    )
}

fn declaration(tool: ToolSpec) -> tau_proto::ToolRegistrationDeclared {
    tau_proto::ToolRegistrationDeclared {
        tool,
        tool_group: Some(swarm_tool_group()),
        prompt_fragment: None,
    }
}

fn swarm_tool_group() -> tau_proto::ToolGroup {
    tau_proto::ToolGroup {
        name: tau_proto::ToolGroupName::new(TOOL_GROUP_NAME),
        prompt_fragment: None,
    }
}

fn common_spec(name: &str, description: &str, parameters: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(name),
        model_visible_name: Some(tau_proto::ToolName::new(name)),
        description: Some(description.to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(parameters),
        format: None,
        tags: Vec::new(),
        enabled_by_default: false,
        background_support: None,
        examples: Vec::new(),
    }
}

fn blocker_spec() -> ToolSpec {
    common_spec(
        TASK_BLOCKER_TOOL_NAME,
        "Manage this agent's process-memory Tau Swarm blockers. add accepts {action,title,description,recommended_answer?,task_id?} and returns {status:\"active\",blocker_id,revision:1}; cancel accepts {action,blocker_id,reason?} and returns the cancelled record; list accepts only {action} and returns this agent's active, answered, and cancelled records in opening order.",
        serde_json::json!({
            "type":"object",
            "properties":{
                "action":{"type":"string","enum":["add","cancel","list"]},
                "blocker_id":{"type":"string","maxLength":128},
                "title":{"type":"string","maxLength":256},
                "description":{"type":"string","maxLength":65536},
                "recommended_answer":{"type":"string","maxLength":65536},
                "task_id":{"type":"string","maxLength":128},
                "reason":{"type":"string","maxLength":4096}
            },
            "required":["action"],
            "additionalProperties":false
        }),
    )
}

fn update_spec() -> ToolSpec {
    common_spec(
        TASK_UPDATE_TOOL_NAME,
        "Publish an immutable process-memory status update to Tau Swarm.",
        serde_json::json!({
            "type":"object",
            "properties":{
                "title":{"type":"string","maxLength":256},
                "description":{"type":"string","maxLength":65536},
                "task_id":{"type":"string","maxLength":128}
            },
            "required":["title","description"],
            "additionalProperties":false
        }),
    )
}

fn decode<T: serde::de::DeserializeOwned>(value: &CborValue) -> Result<T, String> {
    let json = serde_json::to_value(value).map_err(|_| "tool arguments are not representable")?;
    serde_json::from_value(json).map_err(|error| format!("invalid tool arguments: {error}"))
}

fn handle_blocker(cx: ToolContext<'_, SwarmRuntime>) -> Result<(), ClientError> {
    let args = match decode::<BlockerArgs>(&cx.invoke().arguments) {
        Ok(args) => args,
        Err(error) => return report_error(&cx, error),
    };
    let owner = cx.invoke().agent_id.as_str().to_owned();
    let result = match args {
        BlockerArgs::Add {
            title,
            description,
            recommended_answer,
            task_id,
        } => add_blocker(
            cx.state,
            &owner,
            title,
            description,
            recommended_answer,
            task_id,
        ),
        BlockerArgs::Cancel { blocker_id, reason } => {
            cancel_blocker(cx.state, &owner, blocker_id, reason)
        }
        BlockerArgs::List {} => list_blockers(cx.state, &owner),
    };
    match result {
        Ok(value) => report_json(&cx, value),
        Err(error) => report_error(&cx, error),
    }
}

fn add_blocker(
    state: &mut SwarmRuntime,
    owner: &str,
    title: String,
    description: String,
    recommended_answer: Option<String>,
    task_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let health = state.worker_health.clone();
    let _authority = health.mutation_authority()?;
    require_valid_owner(state, owner)?;
    validate_text("title", &title, 256)?;
    validate_text("description", &description, 65_536)?;
    validate_optional(
        "recommended_answer",
        recommended_answer.as_deref(),
        65_536,
        false,
    )?;
    validate_optional("task_id", task_id.as_deref(), 128, true)?;
    let config = state.config.as_ref().ok_or("Swarm is not configured")?;
    let mut history = state
        .blocker_history
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if config.blocker_history_limits.entries <= history.len() {
        return Err("blocker entry limit is full".into());
    }
    let id = random_id();
    let publication = BlockerPublication {
        blocker_id: BlockerId::new(id.clone()),
        revision: BlockerRevisionNumber(1),
        owner: tau_swarm_api::AgentId::new(owner),
        title: title.clone(),
        description: description.clone(),
        recommended_answer: recommended_answer.clone(),
        task_id: task_id.clone().map(TaskId::new),
        source_timestamp: now(),
    };
    let record = BlockerRecord {
        blocker_id: BlockerId::new(id.clone()),
        revision: BlockerRevisionNumber(1),
        owner: tau_swarm_api::AgentId::new(owner),
        title,
        description,
        recommended_answer,
        task_id: task_id.map(TaskId::new),
        lifecycle: BlockerLifecycle::active(),
    };
    let mut prospective: Vec<_> = history
        .iter()
        .filter(|record| record.owner.as_str() == owner)
        .cloned()
        .collect();
    prospective.push(record.clone());
    if !owner_history_fits(
        &history,
        owner,
        &prospective,
        config.blocker_history_limits.encoded_bytes,
    )? {
        return Err("blocker byte limit is full".into());
    }
    state
        .projection
        .blocking_lock()
        .add_blocker(publication)
        .map_err(str::to_owned)?;
    state.changed.notify_waiters();
    history.push(record);
    Ok(serde_json::json!({"status":"active","blocker_id":id,"revision":1}))
}

fn cancel_blocker(
    state: &mut SwarmRuntime,
    owner: &str,
    id: String,
    reason: Option<String>,
) -> Result<serde_json::Value, String> {
    let health = state.worker_health.clone();
    let _authority = health.mutation_authority()?;
    require_valid_owner(state, owner)?;
    validate_optional("reason", reason.as_deref(), 4_096, false)?;
    let mut history = state
        .blocker_history
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let index = history
        .iter()
        .position(|record| record.blocker_id.as_str() == id && record.owner.as_str() == owner)
        .ok_or("blocker is not owned by this agent")?;
    match history[index].lifecycle {
        BlockerLifecycle::Active {
            pending_answer_bytes: None,
        } => {}
        BlockerLifecycle::Active {
            pending_answer_bytes: Some(_),
        } => return Err("blocker answer is already pending".into()),
        BlockerLifecycle::Answered { .. } | BlockerLifecycle::Cancelled { .. } => {
            return Err("blocker is not active".into());
        }
    }
    let mut prospective: Vec<_> = history
        .iter()
        .filter(|record| record.owner.as_str() == owner)
        .cloned()
        .collect();
    let prospective_record = prospective
        .iter_mut()
        .find(|record| record.blocker_id.as_str() == id)
        .ok_or("blocker is not owned by this agent")?;
    prospective_record.lifecycle = BlockerLifecycle::Cancelled {
        reason: reason.clone(),
    };
    let prospective_lifecycle = prospective_record.lifecycle.clone();
    let limit = state
        .config
        .as_ref()
        .ok_or("Swarm is not configured")?
        .blocker_history_limits
        .encoded_bytes;
    if !owner_history_fits(&history, owner, &prospective, limit)? {
        return Err("blocker byte limit is full".into());
    }
    state
        .projection
        .blocking_lock()
        .remove_blocker(&BlockerId::new(id), reason)
        .map_err(str::to_owned)?;
    state.changed.notify_waiters();
    history[index].lifecycle = prospective_lifecycle;
    serde_json::to_value(&history[index]).map_err(|_| "blocker encoding failed".into())
}

fn owner_history_fits(
    history: &[BlockerRecord],
    owner: &str,
    prospective: &[BlockerRecord],
    limit: usize,
) -> Result<bool, String> {
    let encoded = serde_json::to_vec(prospective)
        .map_err(|_| "blocker history encoding failed")?
        .len();
    let reserved = history
        .iter()
        .filter(|record| record.owner.as_str() == owner)
        .try_fold(0_usize, |total, record| {
            total.checked_add(record.lifecycle.pending_answer_bytes())
        })
        .ok_or_else(|| "blocker byte accounting overflow".to_owned())?;
    let required = encoded
        .checked_add(reserved)
        .ok_or_else(|| "blocker byte accounting overflow".to_owned())?;
    Ok(required <= limit)
}

fn list_blockers(state: &SwarmRuntime, owner: &str) -> Result<serde_json::Value, String> {
    serde_json::to_value(
        state
            .blocker_history
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|record| record.owner.as_str() == owner)
            .collect::<Vec<_>>(),
    )
    .map_err(|_| "blocker history encoding failed".into())
}

fn handle_update(cx: ToolContext<'_, SwarmRuntime>) -> Result<(), ClientError> {
    let args = match decode::<UpdateArgs>(&cx.invoke().arguments) {
        Ok(args) => args,
        Err(error) => return report_error(&cx, error),
    };
    let owner = cx.invoke().agent_id.as_str().to_owned();
    let result = add_update(cx.state, &owner, args);
    match result {
        Ok(value) => report_json(&cx, value),
        Err(error) => report_error(&cx, error),
    }
}

fn handle_task_info(cx: ToolContext<'_, SwarmRuntime>) -> Result<(), ClientError> {
    let args = match decode::<TaskInfoArgs>(&cx.invoke().arguments) {
        Ok(args) => args,
        Err(error) => return report_error(&cx, error),
    };
    let owner = cx.invoke().agent_id.as_str().to_owned();
    let result = replace_task_info(cx.state, &owner, args);
    match result {
        Ok(value) => report_json(&cx, value),
        Err(error) => report_error(&cx, error),
    }
}

fn replace_task_info(
    state: &mut SwarmRuntime,
    owner: &str,
    args: TaskInfoArgs,
) -> Result<serde_json::Value, String> {
    let health = state.worker_health.clone();
    let _authority = health.mutation_authority()?;
    require_valid_owner(state, owner)?;
    let info = canonicalize_task_info(args)?;
    state
        .projection
        .blocking_lock()
        .upsert_task_info(info.clone())
        .map_err(str::to_owned)?;
    state.changed.notify_waiters();
    Ok(serde_json::json!({
        "task_id": info.task_id.as_str(),
        "title": info.title.as_str(),
        "description": info.description.as_ref().map(TaskDescription::as_str),
    }))
}

fn canonicalize_task_info(args: TaskInfoArgs) -> Result<TaskInfo, String> {
    let task_id = TaskId::new(args.task_id);
    tau_swarm_api::validate_task_id(&task_id).map_err(|error| error.to_string())?;
    Ok(TaskInfo {
        task_id,
        title: TaskTitle::new(args.title).map_err(|error| error.to_string())?,
        description: args
            .description
            .map(TaskDescription::new)
            .transpose()
            .map_err(|error| error.to_string())?,
    })
}

fn add_update(
    state: &mut SwarmRuntime,
    owner: &str,
    args: UpdateArgs,
) -> Result<serde_json::Value, String> {
    let health = state.worker_health.clone();
    let _authority = health.mutation_authority()?;
    require_valid_owner(state, owner)?;
    validate_text("title", &args.title, 256)?;
    validate_text("description", &args.description, 65_536)?;
    validate_optional("task_id", args.task_id.as_deref(), 128, true)?;
    let id = random_id();
    let usage = state.projection.blocking_lock().update_usage();
    let config = state.config.as_ref().ok_or("Swarm is not configured")?;
    let added_bytes = id.len()
        + owner.len()
        + args.title.len()
        + args.description.len()
        + args.task_id.as_ref().map_or(0, String::len);
    if config.update_limits.entries <= usage.entries {
        return Err("update entry limit is full".into());
    }
    if config.update_limits.logical_bytes < usage.logical_bytes.saturating_add(added_bytes) {
        return Err("update byte limit is full".into());
    }
    let update = UpdatePublication {
        id: UpdateId::new(id.clone()),
        owner: tau_swarm_api::AgentId::new(owner),
        title: args.title,
        description: args.description,
        task_id: args.task_id.map(TaskId::new),
        source_timestamp: now(),
    };
    state
        .projection
        .blocking_lock()
        .add_update(update)
        .map_err(str::to_owned)?;
    state.changed.notify_waiters();
    Ok(serde_json::json!({"update_id":id}))
}

fn require_valid_owner(state: &SwarmRuntime, owner: &str) -> Result<(), String> {
    if state.projection_valid
        && state.replay_complete
        && state
            .projection
            .blocking_lock()
            .contains_agent(&tau_swarm_api::AgentId::new(owner))
    {
        Ok(())
    } else {
        Err(
            "Tau Swarm owner is unavailable until successful replay has a live publication worker"
                .into(),
        )
    }
}

fn validate_text(name: &str, text: &str, max: usize) -> Result<(), String> {
    if text.trim().is_empty() || max < text.len() {
        Err(format!("{name} must be nonempty and at most {max} bytes"))
    } else {
        Ok(())
    }
}

fn validate_optional(
    name: &str,
    value: Option<&str>,
    max: usize,
    controls: bool,
) -> Result<(), String> {
    if let Some(value) = value
        && (value.is_empty()
            || max < value.len()
            || (controls && value.chars().any(char::is_control)))
    {
        return Err(format!("{name} must contain 1..={max} valid bytes"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;

fn random_id() -> String {
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now() -> Timestamp {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| {
            i64::try_from(value.as_micros()).unwrap_or(i64::MAX)
        });
    Timestamp(micros)
}

fn report_json(
    cx: &ToolContext<'_, SwarmRuntime>,
    value: serde_json::Value,
) -> Result<(), ClientError> {
    let cbor = serde_json::from_value::<CborValue>(value)
        .map_err(|_| ClientError::handler("tool result encoding failed"))?;
    cx.report_result(ToolResult {
        presentation: Default::default(),
        call_id: cx.invoke().call_id.clone(),
        tool_name: cx.invoke().tool_name.clone(),
        tool_type: ToolType::Function,
        result: cbor,
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: Some(ToolUseState {
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: cx.invoke().originator.clone(),
    })
}

fn report_error(cx: &ToolContext<'_, SwarmRuntime>, message: String) -> Result<(), ClientError> {
    cx.report_error(tau_proto::ToolError {
        presentation: Default::default(),
        call_id: cx.invoke().call_id.clone(),
        tool_name: cx.invoke().tool_name.clone(),
        tool_type: ToolType::Function,
        display: Some(ToolUseState {
            status: ToolUseStatus::Error,
            status_text: message.clone(),
            ..Default::default()
        }),
        message,
        details: None,
        originator: cx.invoke().originator.clone(),
    })
}
