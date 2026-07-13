//! Runtime state and reader-loop dispatch after the ext-shell handshake.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use tau_proto::{
    CborValue, Event, ExtensionContextReady, HarnessInputMessage, ToolResult, ToolResultKind,
};
use tracing::debug;

use super::{
    apply_started_cwd_metadata, apply_working_directory, cwd_context_event, cwd_notice_event,
    dir_lock_tool_spec, dispatch_action_invoke, dispatch_session_agent_loaded,
    dispatch_session_started, is_shell_tool, schedule_tool_started, schedule_ui_shell_command,
    send_tool_failure, send_ui_shell_saturated_failure, with_lock_wait_duration,
};
use crate::Output;
use crate::config::ExtConfig;
use crate::cwd_state::CwdState;
use crate::dir_lock::DirLockManager;
use crate::scheduler::WorkScheduler;

pub(super) struct ShellRuntime {
    config: ExtConfig,
    scheduler: Option<WorkScheduler>,
    tx: Output,
    running_calls: Arc<Mutex<HashMap<tau_proto::ToolCallId, mpsc::Sender<()>>>>,
    running_ui_commands: Arc<Mutex<HashMap<tau_proto::ShellCommandId, mpsc::Sender<()>>>>,
    shutdown_generation: Arc<AtomicU64>,
    lock_manager: DirLockManager,
    cwd_state: CwdState,
    start_agent_owners: HashMap<String, tau_proto::AgentId>,
    runtime_started: bool,
}

impl ShellRuntime {
    pub(super) fn new(tx: Output, config: ExtConfig) -> Self {
        Self {
            config,
            scheduler: Some(WorkScheduler::new(tx.clone(), Default::default())),
            tx,
            running_calls: Arc::new(Mutex::new(HashMap::new())),
            running_ui_commands: Arc::new(Mutex::new(HashMap::new())),
            shutdown_generation: Arc::new(AtomicU64::new(0)),
            lock_manager: DirLockManager::default(),
            cwd_state: CwdState::new(),
            start_agent_owners: HashMap::new(),
            runtime_started: false,
        }
    }

    fn scheduler(&self) -> tau_client::ClientResult<&WorkScheduler> {
        self.scheduler
            .as_ref()
            .ok_or_else(|| tau_client::ClientError::handler("shell scheduler is shut down"))
    }

    fn send(&self, message: HarnessInputMessage) -> tau_client::ClientResult<()> {
        self.tx.send(message)
    }

    pub(super) fn shutdown(&mut self) {
        self.shutdown_generation.fetch_add(1, Ordering::SeqCst);
        // Dir-lock waiters must be woken before the scheduler is dropped,
        // because scheduler drop joins workers that may be blocked on locks.
        self.lock_manager.shutdown();
        if let Some(scheduler) = &self.scheduler {
            scheduler.cancel_all_queued();
        }
        let running = self
            .running_calls
            .lock()
            .expect("running call registry lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for cancel_tx in running {
            let _ = cancel_tx.send(());
        }
        let running_ui = self
            .running_ui_commands
            .lock()
            .expect("running ui shell registry lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for cancel_tx in running_ui {
            let _ = cancel_tx.send(());
        }
    }

    pub(super) fn final_shutdown(&mut self) {
        self.shutdown();
        drop(self.scheduler.take());
    }

    pub(super) fn apply_config(
        &mut self,
        instance_name: Option<tau_proto::ExtensionName>,
        mut cfg: ExtConfig,
    ) -> tau_client::ClientResult<()> {
        if cfg.working_directory.is_none() {
            cfg.working_directory = self.config.working_directory.clone();
        }
        if let Some(instance_name) = instance_name.as_ref() {
            self.cwd_state
                .set_instance_name(instance_name.as_str().to_owned());
        }
        if let Err(message) = apply_working_directory(&self.config, &cfg, self.runtime_started) {
            return Err(tau_client::ClientError::handler(message));
        }
        if let Err(message) = self.lock_manager.configure(&cfg.dir_lock) {
            return Err(tau_client::ClientError::handler(message));
        }

        let dir_lock_was_enabled = self.config.dir_lock.enable;
        let dir_lock_changed = dir_lock_was_enabled != cfg.dir_lock.enable;
        let dir_lock_disabling = dir_lock_was_enabled && !cfg.dir_lock.enable;
        self.config = cfg;
        if dir_lock_disabling {
            let _ = self.lock_manager.disable();
        }
        if dir_lock_changed {
            self.tx.register_local_tool(tau_proto::ToolRegister {
                tool: dir_lock_tool_spec(self.config.dir_lock.enable),
                tool_group: Some(tau_proto::ToolGroup {
                    name: tau_proto::ToolGroupName::new("shell"),
                    prompt_fragment: None,
                }),
                prompt_fragment: None,
            })?;
        }
        Ok(())
    }

    pub(super) fn handle_event(
        &mut self,
        event: Event,
        is_replay: bool,
    ) -> tau_client::ClientResult<()> {
        self.runtime_started = true;
        match event {
            Event::AgentStarted(started) => {
                apply_started_cwd_metadata(started, &self.tx, &self.cwd_state, is_replay);
            }
            Event::ToolStarted(invoke) => {
                let local_tool_name = invoke.tool_name.clone();
                self.handle_tool_started(invoke, &local_tool_name, is_replay)?;
            }
            Event::SessionStarted(started) => {
                dispatch_session_started(started, &self.tx);
            }
            Event::SessionAgentLoaded(loaded) => {
                dispatch_session_agent_loaded(loaded, &self.tx, &self.cwd_state, true);
            }
            Event::SessionAgentUnloaded(unloaded) => {
                if !is_replay {
                    self.handle_session_agent_unloaded(unloaded);
                }
            }
            Event::AgentMetadataSet(set) => self.handle_agent_metadata_set(set, is_replay),
            Event::AgentMetadataUnset(unset) => self.handle_agent_metadata_unset(unset, is_replay),
            Event::AgentReplayComplete(done) => self.handle_agent_replay_complete(done),
            Event::SessionShutdown(_) => self.shutdown_session(),
            Event::StartAgentAccepted(accepted) => {
                self.start_agent_owners
                    .insert(accepted.query_id, accepted.agent_id);
            }
            Event::StartAgentResult(result) => self.handle_start_agent_result(result),
            Event::ActionInvoke(invoke) => {
                self.send(HarnessInputMessage::emit(dispatch_action_invoke(
                    invoke,
                    &self.lock_manager,
                )))?;
            }
            Event::ToolCancelRequest(request) => self.handle_tool_cancel_request(request),
            Event::UiShellCommand(cmd) => self.handle_ui_shell_command(cmd)?,
            _ => {}
        }
        Ok(())
    }

    fn handle_tool_started(
        &self,
        invoke: tau_proto::ToolStarted,
        local_tool_name: &tau_proto::ToolName,
        is_replay: bool,
    ) -> tau_client::ClientResult<()> {
        if is_replay || !is_shell_tool(local_tool_name.as_str()) {
            return Ok(());
        }
        if let Err(error) = schedule_tool_started(
            (invoke, local_tool_name),
            self.scheduler()?,
            &self.tx,
            self.config.clone(),
            self.lock_manager.clone(),
            Arc::clone(&self.running_calls),
            self.cwd_state.clone(),
        ) {
            let (invoke, failure) = *error;
            send_tool_failure(invoke, failure, &self.tx);
        }
        Ok(())
    }

    pub(super) fn handle_scoped_tool_started(
        &mut self,
        invoke: tau_proto::ToolStarted,
        local_tool_name: &tau_proto::ToolName,
    ) -> tau_client::ClientResult<()> {
        self.runtime_started = true;
        self.handle_tool_started(invoke, local_tool_name, false)
    }

    fn handle_session_agent_unloaded(&mut self, unloaded: tau_proto::SessionAgentUnloaded) {
        self.lock_manager.release_agent(&unloaded.agent_id);
        if let Some(scheduler) = &self.scheduler {
            scheduler.cancel_agent(&unloaded.agent_id);
        }
        self.cwd_state.unset(&unloaded.agent_id);
        self.cwd_state.take_pending_ready(&unloaded.agent_id);
        self.cwd_state.take_pending_notice(&unloaded.agent_id);
        self.cwd_state.take_pending_cd_result(&unloaded.agent_id);
        self.start_agent_owners
            .retain(|_, agent_id| agent_id != &unloaded.agent_id);
    }

    fn handle_agent_metadata_set(&mut self, set: tau_proto::AgentMetadataSet, is_replay: bool) {
        if set.key != self.cwd_state.key() {
            return;
        }
        if let CborValue::Text(path) = set.value {
            self.handle_text_cwd_metadata_set(set.agent_id, PathBuf::from(path), is_replay);
        } else {
            self.handle_invalid_cwd_metadata_set(set.agent_id, is_replay);
        }
    }

    fn handle_text_cwd_metadata_set(
        &mut self,
        agent_id: tau_proto::AgentId,
        cwd: PathBuf,
        is_replay: bool,
    ) {
        self.cwd_state.set(agent_id.clone(), cwd.clone());
        if is_replay {
            return;
        }
        let _ = self.tx.send(HarnessInputMessage::emit(cwd_context_event(
            agent_id.clone(),
            &cwd,
        )));
        if self.cwd_state.take_pending_notice(&agent_id).is_some() {
            let _ = self.tx.send(HarnessInputMessage::emit(cwd_notice_event(
                agent_id.clone(),
                &cwd,
            )));
        }
        self.complete_pending_cd_after_text_metadata(&agent_id, &cwd);
        self.publish_ready_if_pending(agent_id);
    }

    fn complete_pending_cd_after_text_metadata(
        &self,
        agent_id: &tau_proto::AgentId,
        cwd: &PathBuf,
    ) {
        if let Some(pending_cd) = self
            .cwd_state
            .take_committed_pending_cd_result(agent_id, cwd)
        {
            let event = if pending_cd.matched_request {
                let output = crate::tools::cd::output(cwd);
                Event::ToolResult(ToolResult {
                    call_id: pending_cd.invoke.call_id,
                    tool_name: pending_cd.invoke.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                    result: output.result,
                    provider_content: Vec::new(),
                    kind: ToolResultKind::Final,
                    display: Some(output.display),
                    originator: pending_cd.invoke.originator,
                })
            } else {
                Event::ToolError(tau_proto::ToolError {
                    call_id: pending_cd.invoke.call_id,
                    tool_name: pending_cd.invoke.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                    message: format!(
                        "committed cwd metadata did not match requested cwd; cwd changed to {}",
                        cwd.display()
                    ),
                    details: None,
                    display: None,
                    originator: pending_cd.invoke.originator,
                })
            };
            let _ = self
                .tx
                .send(HarnessInputMessage::emit(with_lock_wait_duration(
                    event,
                    pending_cd.lock_wait_duration_seconds,
                )));
        }
    }

    fn handle_invalid_cwd_metadata_set(&mut self, agent_id: tau_proto::AgentId, is_replay: bool) {
        if is_replay {
            return;
        }
        let cwd = self.cwd_state.get_or_default(&agent_id);
        let _ = self.tx.send(HarnessInputMessage::emit(cwd_context_event(
            agent_id.clone(),
            &cwd,
        )));
        self.cwd_state.take_pending_notice(&agent_id);
        self.complete_pending_cd_with_error(
            &agent_id,
            "committed cwd metadata value is not text; cwd unchanged",
        );
        self.publish_ready_if_pending(agent_id);
    }

    fn handle_agent_metadata_unset(
        &mut self,
        unset: tau_proto::AgentMetadataUnset,
        is_replay: bool,
    ) {
        if unset.key != self.cwd_state.key() {
            return;
        }
        self.cwd_state.unset(&unset.agent_id);
        if is_replay {
            return;
        }
        let cwd = self.cwd_state.get_or_default(&unset.agent_id);
        let _ = self.tx.send(HarnessInputMessage::emit(cwd_context_event(
            unset.agent_id.clone(),
            &cwd,
        )));
        self.cwd_state.take_pending_notice(&unset.agent_id);
        self.complete_pending_cd_with_error(
            &unset.agent_id,
            "committed cwd metadata was unset; cwd reverted to the process default",
        );
        self.publish_ready_if_pending(unset.agent_id);
    }

    fn complete_pending_cd_with_error(&self, agent_id: &tau_proto::AgentId, message: &str) {
        if let Some(pending_cd) = self.cwd_state.take_pending_cd_result(agent_id) {
            let event = Event::ToolError(tau_proto::ToolError {
                call_id: pending_cd.invoke.call_id,
                tool_name: pending_cd.invoke.tool_name,
                tool_type: tau_proto::ToolType::Function,
                message: message.to_owned(),
                details: None,
                display: None,
                originator: pending_cd.invoke.originator,
            });
            let _ = self
                .tx
                .send(HarnessInputMessage::emit(with_lock_wait_duration(
                    event,
                    pending_cd.lock_wait_duration_seconds,
                )));
        }
    }

    fn publish_ready_if_pending(&self, agent_id: tau_proto::AgentId) {
        if let Some(session_id) = self.cwd_state.take_pending_ready(&agent_id) {
            let _ = self
                .tx
                .send(HarnessInputMessage::emit(Event::ExtensionContextReady(
                    ExtensionContextReady {
                        session_id,
                        agent_id,
                    },
                )));
        }
    }

    fn handle_agent_replay_complete(&self, done: tau_proto::AgentReplayComplete) {
        let Some(session_id) = self.cwd_state.pending_ready(&done.agent_id) else {
            return;
        };
        if done.error.is_some() {
            self.cwd_state.take_pending_ready(&done.agent_id);
            return;
        }
        if let Some(cwd) = self.cwd_state.get(&done.agent_id) {
            let _ = self.tx.send(HarnessInputMessage::emit(cwd_context_event(
                done.agent_id.clone(),
                &cwd,
            )));
            self.publish_ready_if_pending(done.agent_id);
            return;
        }
        let cwd = CwdState::process_default();
        let _ = self
            .tx
            .send(HarnessInputMessage::emit(Event::AgentMetadataSet(
                tau_proto::AgentMetadataSet {
                    agent_id: done.agent_id.clone(),
                    key: self.cwd_state.key(),
                    value: CborValue::Text(cwd.display().to_string()),
                    inheritable: true,
                },
            )));
        self.cwd_state.set_pending_ready(done.agent_id, session_id);
    }

    fn shutdown_session(&mut self) {
        self.shutdown();
        self.start_agent_owners.clear();
    }

    fn handle_start_agent_result(&mut self, result: tau_proto::StartAgentResult) {
        if let Some(agent_id) = self.start_agent_owners.remove(&result.query_id) {
            self.lock_manager.release_agent(&agent_id);
            if let Some(scheduler) = &self.scheduler {
                scheduler.cancel_agent(&agent_id);
            }
        }
    }

    fn handle_tool_cancel_request(&self, request: tau_proto::ToolCancelRequest) {
        if self
            .scheduler
            .as_ref()
            .is_some_and(|scheduler| scheduler.cancel_queued_call(&request.target_call_id))
        {
            debug!(call_id = %request.target_call_id, "cancellation requested for queued shell work");
            return;
        }
        let cancel_tx = self
            .running_calls
            .lock()
            .expect("running call registry lock poisoned")
            .get(&request.target_call_id)
            .cloned();
        if let Some(cancel_tx) = cancel_tx {
            debug!(call_id = %request.target_call_id, "tool cancellation requested for running call");
            if cancel_tx.send(()).is_err() {
                debug!(call_id = %request.target_call_id, "shell cancellation receiver already gone");
            }
        } else if self
            .lock_manager
            .cancel_waiting_call(&request.target_call_id)
        {
            debug!(call_id = %request.target_call_id, "cancellation requested for waiting dir-lock call");
        } else {
            debug!(call_id = %request.target_call_id, "tool cancellation requested for unknown call");
        }
    }

    fn handle_ui_shell_command(
        &self,
        cmd: tau_proto::UiShellCommand,
    ) -> tau_client::ClientResult<()> {
        if let Err(error) = schedule_ui_shell_command(
            cmd,
            self.scheduler()?,
            &self.tx,
            self.config.shell.clone(),
            Arc::clone(&self.running_ui_commands),
            Arc::clone(&self.shutdown_generation),
            self.shutdown_generation.load(Ordering::SeqCst),
        ) {
            let (cmd, message) = *error;
            send_ui_shell_saturated_failure(cmd, message, &self.tx);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures ToolCancelRequest reaches already-running cancellable tool
    /// calls, not just queued scheduler work or shell-only registry
    /// entries.
    #[test]
    fn tool_cancel_request_signals_registered_running_call() {
        let (tx, _rx) = mpsc::channel();
        let runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
        let call_id = tau_proto::ToolCallId::new("running-find");
        let (cancel_tx, cancel_rx) = mpsc::channel();
        runtime
            .running_calls
            .lock()
            .expect("running call registry")
            .insert(call_id.clone(), cancel_tx);

        runtime.handle_tool_cancel_request(tau_proto::ToolCancelRequest {
            target_call_id: call_id,
        });

        cancel_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .expect("running call cancel signal");
    }

    /// Ensures runtime shutdown signals registered running cancellable tool
    /// calls before scheduler drop waits for worker jobs to exit.
    #[test]
    fn shutdown_signals_registered_running_call() {
        let (tx, _rx) = mpsc::channel();
        let mut runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
        let call_id = tau_proto::ToolCallId::new("running-grep");
        let (cancel_tx, cancel_rx) = mpsc::channel();
        runtime
            .running_calls
            .lock()
            .expect("running call registry")
            .insert(call_id, cancel_tx);

        runtime.shutdown();

        cancel_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .expect("shutdown cancel signal");
    }

    /// Ensures replayed cwd metadata is folded for later boundary-approved
    /// context readiness without emitting replay-time side effects.
    #[test]
    fn replayed_cwd_metadata_folds_without_emitting_until_live_agent_load() {
        let (tx, rx) = mpsc::channel();
        let mut runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
        let agent_id = tau_proto::AgentId::parse("agent-replay-cwd").expect("agent id");
        let cwd = std::env::current_dir().expect("current dir");

        runtime
            .handle_event(
                Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                    agent_id: agent_id.clone(),
                    key: runtime.cwd_state.key(),
                    value: CborValue::Text(cwd.display().to_string()),
                    inheritable: true,
                }),
                true,
            )
            .expect("replay metadata");
        assert!(rx.try_recv().is_err(), "replay fold must not emit output");

        runtime
            .handle_event(
                Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                    session_id: "session-1".into(),
                    agent_id: agent_id.clone(),
                    ephemeral: false,
                }),
                false,
            )
            .expect("live load");
        assert!(
            rx.try_recv().is_err(),
            "live load waits for replay boundary before emitting"
        );
        runtime
            .handle_event(
                Event::AgentReplayComplete(tau_proto::AgentReplayComplete {
                    agent_id: agent_id.clone(),
                    session_id: Some("session-1".into()),
                    error: None,
                }),
                false,
            )
            .expect("replay boundary");

        let HarnessInputMessage::Emit(context) = rx.recv().expect("context publish") else {
            panic!("expected context publish");
        };
        assert!(matches!(
            context.event.as_ref(),
            Event::ExtAgentContextPublish(publish)
                if publish.agent_id == agent_id && publish.key.as_ref() == "cwd"
        ));
        let HarnessInputMessage::Emit(ready) = rx.recv().expect("context ready") else {
            panic!("expected context ready");
        };
        assert!(matches!(
            ready.event.as_ref(),
            Event::ExtensionContextReady(ready)
                if ready.agent_id == agent_id && ready.session_id == "session-1"
        ));
    }

    /// Ensures a session-level shutdown cleans shell-owned state without
    /// dropping the scheduler needed by a subsequent session in the same
    /// extension process.
    #[test]
    fn session_shutdown_keeps_scheduler_for_later_sessions() {
        let (tx, _rx) = mpsc::channel();
        let mut runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());

        runtime
            .handle_event(
                Event::SessionShutdown(tau_proto::SessionShutdown {
                    session_id: "session-1".into(),
                }),
                false,
            )
            .expect("session shutdown");

        assert!(runtime.scheduler.is_some());
    }
}
