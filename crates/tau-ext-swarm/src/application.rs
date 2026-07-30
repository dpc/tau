use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tau_swarm_api::{AgentId, SessionIdentity, UpdateId, UpdatePublication};
use tau_swarm_client::{
    Application, ChangeBatch, ClientError, ClientResult, PublicationRevision, RevisionedSnapshot,
};
use tau_swarm_client_api::{
    AnswerBlockerRequest, AnswerBlockerResponse, DeliverPromptRequest, DeliverPromptResponse,
    v2 as path_tau_swarm_client_api_v2,
};
use tokio::sync::{Mutex, Notify, mpsc, oneshot, watch};

use crate::projection::SessionProjection;
use crate::tools::{BlockerRecord, BlockerState};

/// One request for Tau's existing internal prompt submission path.
#[derive(Debug)]
pub(crate) struct PromptSubmission {
    /// Target Tau agent.
    pub agent_id: AgentId,
    /// Exact prompt text.
    pub text: String,
    /// Correlation identifier used as Tau's `ctx_id`.
    pub ctx_id: String,
    /// Completion sent after the matching canonical prompt event appears.
    pub completion: oneshot::Sender<Result<(), String>>,
}

/// One blocker answer sent through Tau's existing internal prompt path.
#[derive(Debug)]
pub(crate) struct BlockerSubmission {
    /// Agent that owns the blocker.
    pub agent_id: AgentId,
    /// Exact XML prompt body.
    pub text: String,
    /// Blocker command identifier used as Tau's `ctx_id`.
    pub ctx_id: String,
    /// Completion sent after the matching canonical prompt event appears.
    pub completion: oneshot::Sender<Result<(), String>>,
}

enum CommandEntry {
    /// Prompt command and its joinable terminal outcome.
    Prompt {
        /// Full immutable payload bound to this ID.
        request: DeliverPromptRequest,
        /// In-flight or cached terminal outcome.
        result: watch::Receiver<Option<Result<DeliverPromptResponse, String>>>,
    },
    /// Blocker-answer command and its joinable terminal outcome.
    Blocker {
        /// Full immutable payload bound to this ID.
        request: AnswerBlockerRequest,
        /// In-flight or cached terminal outcome.
        result: watch::Receiver<Option<Result<AnswerBlockerResponse, String>>>,
    },
}

/// Bounded no-eviction command table shared across remote command kinds.
pub(crate) struct CommandState {
    /// Tagged no-eviction table shared by both remote command kinds.
    entries: HashMap<(SessionIdentity, String), CommandEntry>,
    /// Retained logical request/result string bytes.
    bytes: usize,
    /// Configured no-eviction entry ceiling.
    maximum_entries: usize,
    /// Configured no-eviction logical byte ceiling.
    maximum_bytes: usize,
}

impl CommandState {
    /// Creates an empty process-incarnation command table under configured
    /// bounds.
    pub(crate) fn new(maximum_entries: usize, maximum_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            maximum_entries,
            maximum_bytes,
        }
    }
}

/// Tau-owned implementation consumed by the reconnecting Swarm client.
pub(crate) struct SwarmApplication {
    /// Stable current session identity.
    identity: SessionIdentity,
    /// Coherent shared projection.
    projection: Arc<Mutex<SessionProjection>>,
    /// Wakes change waiters after projection mutations.
    changed: Arc<Notify>,
    /// Sends prompts to the nonblocking Tau protocol owner.
    prompts: mpsc::Sender<PromptSubmission>,
    /// Sends blocker answers to the nonblocking Tau protocol owner.
    blockers: mpsc::Sender<BlockerSubmission>,
    /// In-flight and completed commands keyed across both command kinds.
    commands: Arc<Mutex<CommandState>>,
    /// Full owner-visible blocker lifecycle history.
    blocker_history: Option<Arc<std::sync::Mutex<Vec<BlockerRecord>>>>,
    /// End-to-end queue-admission and canonical-loopback deadline.
    command_timeout: Duration,
    /// Maximum encoded full-history blocker listing.
    blocker_history_bytes: usize,
}

impl SwarmApplication {
    /// Creates an application over a projection and prompt-loopback channel.
    #[must_use]
    pub fn new(
        identity: SessionIdentity,
        projection: Arc<Mutex<SessionProjection>>,
        changed: Arc<Notify>,
        prompts: mpsc::Sender<PromptSubmission>,
        blockers: mpsc::Sender<BlockerSubmission>,
    ) -> Self {
        Self {
            identity,
            projection,
            changed,
            prompts,
            blockers,
            commands: Arc::new(Mutex::new(CommandState::new(1_024, 16 * 1024 * 1024))),
            blocker_history: None,
            command_timeout: Duration::from_secs(25),
            blocker_history_bytes: 4 * 1024 * 1024,
        }
    }

    /// Applies the configured command deadline and no-eviction table bounds.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_command_policy(
        mut self,
        timeout: Duration,
        maximum_entries: usize,
        maximum_bytes: usize,
    ) -> Self {
        self.command_timeout = timeout;
        self.commands = Arc::new(Mutex::new(CommandState::new(
            maximum_entries,
            maximum_bytes,
        )));
        self
    }

    /// Installs the process-incarnation command table retained across workers.
    #[must_use]
    pub(crate) fn with_command_state(
        mut self,
        commands: Arc<Mutex<CommandState>>,
        timeout: Duration,
    ) -> Self {
        self.command_timeout = timeout;
        self.commands = commands;
        self
    }

    fn admit_command(commands: &mut CommandState, bytes: usize) -> Result<(), &'static str> {
        let Some(next_bytes) = commands.bytes.checked_add(bytes) else {
            return Err("command byte accounting overflow");
        };
        if commands.maximum_entries <= commands.entries.len() {
            return Err("command entry limit is full");
        }
        if commands.maximum_bytes < next_bytes {
            return Err("command byte limit is full");
        }
        commands.bytes = next_bytes;
        Ok(())
    }

    /// Installs the process-memory blocker history updated after canonical
    /// answer loopback.
    #[must_use]
    pub(crate) fn with_blocker_history(
        mut self,
        history: Arc<std::sync::Mutex<Vec<BlockerRecord>>>,
        maximum_bytes: usize,
    ) -> Self {
        self.blocker_history = Some(history);
        self.blocker_history_bytes = maximum_bytes;
        self
    }
}

#[async_trait]
impl Application for SwarmApplication {
    async fn identity(&self) -> ClientResult<SessionIdentity> {
        Ok(self.identity.clone())
    }
    async fn snapshot(&self) -> ClientResult<RevisionedSnapshot> {
        Ok(self.projection.lock().await.snapshot())
    }
    async fn pending_updates_through(
        &self,
        revision: PublicationRevision,
    ) -> ClientResult<Vec<UpdatePublication>> {
        Ok(self
            .projection
            .lock()
            .await
            .pending_updates_through(revision))
    }
    async fn acknowledge_update(&self, update: &UpdateId) -> ClientResult<()> {
        self.projection.lock().await.acknowledge_update(update);
        Ok(())
    }
    async fn changes_after(&self, revision: PublicationRevision) -> ClientResult<ChangeBatch> {
        loop {
            let notified = self.changed.notified();
            if let Some(batch) = self.projection.lock().await.changes_after(revision) {
                if revision < batch.revision {
                    return Ok(batch);
                }
            } else {
                return Err(ClientError::transport(
                    "Swarm reader fell behind retained changes",
                ));
            }
            notified.await;
        }
    }
    async fn deliver_prompt(
        &self,
        request: DeliverPromptRequest,
    ) -> ClientResult<DeliverPromptResponse> {
        let key = request.prompt.correlation_id.as_str().to_owned();
        let scoped_key = (self.identity.clone(), key.clone());
        if let Err(reason) = validate_id("correlation ID", &key) {
            return Ok(DeliverPromptResponse::Rejected(reason));
        }
        if 256 * 1024 < request.prompt.message.len() {
            return Ok(DeliverPromptResponse::Rejected(
                "prompt message exceeds 262144 bytes".into(),
            ));
        }
        if tau_proto::AgentId::parse(&request.prompt.agent_id).is_err() {
            return Ok(DeliverPromptResponse::Rejected(
                "target agent ID is invalid".into(),
            ));
        }
        let mut commands = self.commands.lock().await;
        if let Some(command) = commands.entries.get(&scoped_key) {
            let CommandEntry::Prompt {
                request: existing,
                result,
            } = command
            else {
                return Ok(DeliverPromptResponse::Rejected(
                    "command ID is already bound to a blocker answer".into(),
                ));
            };
            if existing != &request {
                return Ok(DeliverPromptResponse::Rejected(
                    "correlation ID reused with a different prompt".into(),
                ));
            }
            let result = result.clone();
            drop(commands);
            return wait_for_prompt(result).await;
        }
        if !self
            .projection
            .lock()
            .await
            .contains_agent(&AgentId::new(request.prompt.agent_id.clone()))
        {
            return Ok(DeliverPromptResponse::Rejected(
                "target agent is not loaded".into(),
            ));
        }
        let retained_bytes = match prompt_retained_bytes(&request)
            .and_then(|bytes| session_scoped_retained_bytes(&self.identity, bytes))
        {
            Ok(bytes) => bytes,
            Err(reason) => return Ok(DeliverPromptResponse::Rejected(reason.into())),
        };
        if let Err(reason) = Self::admit_command(&mut commands, retained_bytes) {
            return Ok(DeliverPromptResponse::Rejected(reason.into()));
        }
        let (result_tx, result_rx) = watch::channel(None);
        commands.entries.insert(
            scoped_key,
            CommandEntry::Prompt {
                request: request.clone(),
                result: result_rx,
            },
        );
        drop(commands);
        let operation = async {
            let (completion, accepted) = oneshot::channel();
            self.prompts
                .send(PromptSubmission {
                    agent_id: AgentId::new(request.prompt.agent_id.clone()),
                    text: request.prompt.message.clone(),
                    ctx_id: key.clone(),
                    completion,
                })
                .await
                .map_err(|_| "Tau prompt loopback stopped")?;
            match accepted.await {
                Ok(Ok(())) => Ok(DeliverPromptResponse::Accepted),
                Ok(Err(reason)) => Ok(DeliverPromptResponse::Rejected(bounded_result(reason))),
                Err(_) => Err("prompt acceptance became indeterminate"),
            }
        };
        let result = tokio::time::timeout(self.command_timeout, operation)
            .await
            .unwrap_or(Err("prompt acceptance timed out"))
            .map_err(str::to_owned);
        let _ = result_tx.send(Some(result.clone()));
        result.map_err(ClientError::transport)
    }
    async fn answer_blocker(
        &self,
        request: AnswerBlockerRequest,
    ) -> ClientResult<AnswerBlockerResponse> {
        let key = request.command_id.clone();
        let scoped_key = (self.identity.clone(), key.clone());
        if let Err(reason) = validate_id("command ID", &key)
            .and_then(|()| validate_id("blocker ID", &request.blocker_id))
        {
            return Ok(AnswerBlockerResponse::Rejected(reason));
        }
        if 64 * 1024 < request.response.len() {
            return Ok(AnswerBlockerResponse::Rejected(
                "blocker answer exceeds 65536 bytes".into(),
            ));
        }
        let mut commands = self.commands.lock().await;
        if let Some(command) = commands.entries.get(&scoped_key) {
            let CommandEntry::Blocker {
                request: existing,
                result,
            } = command
            else {
                return Ok(AnswerBlockerResponse::Rejected(
                    "command ID is already bound to a prompt".into(),
                ));
            };
            if existing != &request {
                return Ok(AnswerBlockerResponse::Rejected(
                    "command ID reused with a different blocker answer".into(),
                ));
            }
            let result = result.clone();
            drop(commands);
            return wait_for_blocker(result).await;
        }
        let projection = self.projection.lock().await;
        let blocker = projection.blocker(&request.blocker_id, request.revision);
        let owner = match blocker.as_ref() {
            Some(blocker) => blocker.owner.clone(),
            None => {
                return Ok(AnswerBlockerResponse::Rejected(
                    "blocker revision is not active".into(),
                ));
            }
        };
        if !projection.contains_agent(&owner) {
            return Ok(AnswerBlockerResponse::Rejected(
                "blocker owner is not loaded".into(),
            ));
        }
        drop(projection);
        match request.kind {
            path_tau_swarm_client_api_v2::BlockerAnswerKind::ApprovedRecommendation
                if blocker
                    .as_ref()
                    .and_then(|value| value.recommended_answer.as_ref())
                    != Some(&request.response) =>
            {
                return Ok(AnswerBlockerResponse::Rejected(
                    "approved recommendation must exactly match the active recommendation".into(),
                ));
            }
            path_tau_swarm_client_api_v2::BlockerAnswerKind::Custom
                if request.response.is_empty() =>
            {
                return Ok(AnswerBlockerResponse::Rejected(
                    "custom blocker answer must be nonempty".into(),
                ));
            }
            _ => {}
        }
        let answer_kind = match request.kind {
            path_tau_swarm_client_api_v2::BlockerAnswerKind::ApprovedRecommendation => {
                "approved_recommendation"
            }
            path_tau_swarm_client_api_v2::BlockerAnswerKind::Custom => "custom",
        };
        let text = blocker_answer_xml(
            &request.blocker_id,
            request.revision,
            answer_kind,
            &request.response,
        );
        let retained_bytes = match blocker_retained_bytes(&request, &owner, &text)
            .and_then(|bytes| session_scoped_retained_bytes(&self.identity, bytes))
        {
            Ok(bytes) => bytes,
            Err(reason) => return Ok(AnswerBlockerResponse::Rejected(reason.into())),
        };
        if let Err(reason) = self.reserve_blocker_answer(&request) {
            return Ok(AnswerBlockerResponse::Rejected(reason));
        }
        if let Err(reason) = Self::admit_command(&mut commands, retained_bytes) {
            self.release_blocker_answer(&request);
            return Ok(AnswerBlockerResponse::Rejected(reason.into()));
        }
        let (result_tx, result_rx) = watch::channel(None);
        commands.entries.insert(
            scoped_key,
            CommandEntry::Blocker {
                request: request.clone(),
                result: result_rx,
            },
        );
        drop(commands);
        let operation = async {
            let (completion, accepted) = oneshot::channel();
            self.blockers
                .send(BlockerSubmission {
                    agent_id: owner,
                    text,
                    ctx_id: key,
                    completion,
                })
                .await
                .map_err(|_| "Tau blocker loopback stopped")?;
            match accepted.await {
                Ok(Ok(())) => {
                    self.projection.lock().await.close_answered_blocker(
                        &tau_swarm_api::BlockerId::new(&request.blocker_id),
                    );
                    self.changed.notify_waiters();
                    if let Some(history) = &self.blocker_history
                        && let Some(record) = history
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .iter_mut()
                            .find(|record| {
                                record.blocker_id.as_str() == request.blocker_id
                                    && record.revision.0 == request.revision
                            })
                    {
                        record.state = BlockerState::Answered;
                        record.answer = Some(request.response.clone());
                        record.answer_kind = Some(match request.kind {
                            path_tau_swarm_client_api_v2::BlockerAnswerKind::ApprovedRecommendation => {
                                tau_swarm_api::BlockerAnswerKind::ApprovedRecommendation
                            }
                            path_tau_swarm_client_api_v2::BlockerAnswerKind::Custom => {
                                tau_swarm_api::BlockerAnswerKind::Custom
                            }
                        });
                        record.reserved_answer_bytes = 0;
                    }
                    Ok(AnswerBlockerResponse::Accepted)
                }
                Ok(Err(reason)) => Ok(AnswerBlockerResponse::Rejected(bounded_result(reason))),
                Err(_) => Err("blocker acceptance became indeterminate"),
            }
        };
        let result = tokio::time::timeout(self.command_timeout, operation)
            .await
            .unwrap_or(Err("blocker acceptance timed out"))
            .map_err(str::to_owned);
        if result.is_err() || matches!(result, Ok(AnswerBlockerResponse::Rejected(_))) {
            self.release_blocker_answer(&request);
        }
        let _ = result_tx.send(Some(result.clone()));
        result.map_err(ClientError::transport)
    }
}

impl SwarmApplication {
    fn reserve_blocker_answer(&self, request: &AnswerBlockerRequest) -> Result<(), String> {
        let Some(history) = &self.blocker_history else {
            return Ok(());
        };
        let mut history = history.lock().unwrap_or_else(|error| error.into_inner());
        let index = history
            .iter()
            .position(|record| {
                record.blocker_id.as_str() == request.blocker_id
                    && record.revision.0 == request.revision
            })
            .ok_or_else(|| "blocker revision is not active".to_owned())?;
        if !matches!(history[index].state, BlockerState::Active) {
            return Err("blocker revision is not active".into());
        }
        if history[index].reserved_answer_bytes != 0 {
            return Err("blocker answer is already pending".into());
        }
        let owner = history[index].owner.clone();
        let owner_history: Vec<_> = history
            .iter()
            .filter(|record| record.owner == owner)
            .cloned()
            .collect();
        let before = serde_json::to_vec(&owner_history)
            .map_err(|_| "blocker history encoding failed".to_owned())?
            .len();
        let mut prospective = owner_history;
        let record = prospective
            .iter_mut()
            .find(|record| record.blocker_id.as_str() == request.blocker_id)
            .ok_or_else(|| "blocker revision is not active".to_owned())?;
        record.state = BlockerState::Answered;
        record.answer = Some(request.response.clone());
        record.answer_kind = Some(match request.kind {
            path_tau_swarm_client_api_v2::BlockerAnswerKind::ApprovedRecommendation => {
                tau_swarm_api::BlockerAnswerKind::ApprovedRecommendation
            }
            path_tau_swarm_client_api_v2::BlockerAnswerKind::Custom => {
                tau_swarm_api::BlockerAnswerKind::Custom
            }
        });
        let after = serde_json::to_vec(&prospective)
            .map_err(|_| "blocker history encoding failed".to_owned())?
            .len();
        let reserved_elsewhere = history
            .iter()
            .filter(|record| record.owner == owner)
            .try_fold(0_usize, |total, record| {
                total.checked_add(record.reserved_answer_bytes)
            })
            .ok_or_else(|| "blocker byte accounting overflow".to_owned())?;
        let additional = after
            .checked_sub(before)
            .ok_or_else(|| "blocker byte accounting underflow".to_owned())?;
        let required = before
            .checked_add(reserved_elsewhere)
            .and_then(|bytes| bytes.checked_add(additional))
            .ok_or_else(|| "blocker byte accounting overflow".to_owned())?;
        if self.blocker_history_bytes < required {
            return Err("blocker byte limit is full".into());
        }
        history[index].reserved_answer_bytes = additional.max(1);
        Ok(())
    }

    fn release_blocker_answer(&self, request: &AnswerBlockerRequest) {
        if let Some(history) = &self.blocker_history
            && let Some(record) = history
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter_mut()
                .find(|record| {
                    record.blocker_id.as_str() == request.blocker_id
                        && record.revision.0 == request.revision
                })
        {
            record.reserved_answer_bytes = 0;
        }
    }
}

async fn wait_for_prompt(
    mut result: watch::Receiver<Option<Result<DeliverPromptResponse, String>>>,
) -> ClientResult<DeliverPromptResponse> {
    loop {
        if let Some(response) = result.borrow().clone() {
            return response.map_err(ClientError::transport);
        }
        result
            .changed()
            .await
            .map_err(|_| ClientError::transport("prompt acceptance became indeterminate"))?;
    }
}

async fn wait_for_blocker(
    mut result: watch::Receiver<Option<Result<AnswerBlockerResponse, String>>>,
) -> ClientResult<AnswerBlockerResponse> {
    loop {
        if let Some(response) = result.borrow().clone() {
            return response.map_err(ClientError::transport);
        }
        result
            .changed()
            .await
            .map_err(|_| ClientError::transport("blocker acceptance became indeterminate"))?;
    }
}

fn prompt_retained_bytes(request: &DeliverPromptRequest) -> Result<usize, &'static str> {
    // The command table owns the request and map key. The pending canonical
    // matcher and detached Tau event each own agent/context/text copies.
    checked_weighted_strings(&[
        (request.prompt.correlation_id.as_str(), 4),
        (&request.prompt.agent_id, 3),
        (&request.prompt.message, 3),
    ])?
    .checked_add(MAXIMUM_CACHED_RESULT_BYTES)
    .ok_or("command byte accounting overflow")
}

fn session_scoped_retained_bytes(
    identity: &SessionIdentity,
    command_bytes: usize,
) -> Result<usize, &'static str> {
    command_bytes
        .checked_add(identity.hostname.as_str().len())
        .and_then(|bytes| bytes.checked_add(identity.session_id.as_str().len()))
        .ok_or("command byte accounting overflow")
}

fn blocker_retained_bytes(
    request: &AnswerBlockerRequest,
    owner: &AgentId,
    loopback_text: &str,
) -> Result<usize, &'static str> {
    let strings = checked_weighted_strings(&[
        (&request.command_id, 4),
        (&request.blocker_id, 1),
        (&request.response, 1),
        (owner.as_str(), 2),
        (loopback_text, 2),
    ])?;
    strings
        .checked_add(std::mem::size_of_val(&request.revision))
        .and_then(|bytes| bytes.checked_add(MAXIMUM_CACHED_RESULT_BYTES))
        .ok_or("command byte accounting overflow")
}

const MAXIMUM_CACHED_RESULT_BYTES: usize = 4 * 1024;

fn checked_weighted_strings(values: &[(&str, usize)]) -> Result<usize, &'static str> {
    values.iter().try_fold(0_usize, |total, (value, copies)| {
        value
            .len()
            .checked_mul(*copies)
            .and_then(|bytes| total.checked_add(bytes))
            .ok_or("command byte accounting overflow")
    })
}

fn bounded_result(mut result: String) -> String {
    if result.len() <= MAXIMUM_CACHED_RESULT_BYTES {
        return result;
    }
    let mut end = MAXIMUM_CACHED_RESULT_BYTES;
    while !result.is_char_boundary(end) {
        end -= 1;
    }
    result.truncate(end);
    result
}

fn validate_id(name: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || 128 < value.len() || value.chars().any(char::is_control) {
        Err(format!(
            "{name} must contain 1..=128 bytes without control characters"
        ))
    } else {
        Ok(())
    }
}

fn blocker_answer_xml(blocker_id: &str, revision: u64, answer_kind: &str, answer: &str) -> String {
    let blocker_id = xml_attribute(blocker_id);
    let answer_kind = xml_attribute(answer_kind);
    let answer = answer.replace("</blocker_answer>", "&lt;/blocker_answer>");
    format!(
        "<blocker_answer blocker_id=\"{blocker_id}\" revision=\"{revision}\" answer_kind=\"{answer_kind}\">\n{answer}\n</blocker_answer>"
    )
}

fn xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests;
