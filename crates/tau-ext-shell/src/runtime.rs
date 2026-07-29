//! Runtime state and reader-loop dispatch after the ext-shell handshake.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use tau_proto::{
    CborValue, Event, ExtensionContextReady, HarnessInputMessage, ToolResult, ToolResultKind,
};
use tracing::debug;

use super::{
    UiShellScheduleContext, apply_started_cwd_metadata, apply_working_directory, cwd_context_event,
    cwd_notice_event, dir_lock_tool_spec, dispatch_action_invoke, dispatch_session_agent_loaded,
    dispatch_session_started, invalid_cwd_context_event, is_shell_tool,
    publish_agent_discovery_snapshot_for, schedule_tool_started, schedule_ui_shell_command,
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
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
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
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = cancel_tx.send(());
        }
        self.cwd_state.take_all_pending_workdirs();
    }

    pub(super) fn final_shutdown(&mut self) {
        self.shutdown();
        drop(self.scheduler.take());
        crate::shell_output_spool::shutdown();
    }

    pub(super) fn apply_config(
        &mut self,
        instance_name: tau_proto::ExtensionName,
        tool_prefix: Option<tau_proto::ToolNamePrefix>,
        mut cfg: ExtConfig,
    ) -> tau_client::ClientResult<()> {
        if cfg.working_directory.is_none() {
            cfg.working_directory = self.config.working_directory.clone();
        }
        self.cwd_state
            .set_instance_name(instance_name.as_str().to_owned());
        self.cwd_state.set_context_label(tool_prefix.as_ref());
        if let Err(message) = apply_working_directory(&self.config, &cfg, self.runtime_started) {
            return Err(tau_client::ClientError::handler(message));
        }
        self.cwd_state
            .freeze_process_startup_cwd()
            .map_err(tau_client::ClientError::handler)?;
        if let Err(message) = self.lock_manager.configure(&cfg.dir_lock) {
            return Err(tau_client::ClientError::handler(message));
        }

        let dir_lock_was_enabled = self.config.dir_lock.enable;
        let dir_lock_changed = dir_lock_was_enabled != cfg.dir_lock.enable;
        let dir_lock_disabling = dir_lock_was_enabled && !cfg.dir_lock.enable;
        self.config = cfg;
        if dir_lock_disabling {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = self.lock_manager.disable();
        }
        if dir_lock_changed {
            self.tx
                .register_local_tool(tau_proto::ToolRegistrationDeclared {
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
        self.cwd_state.remove_initialization(&unloaded.agent_id);
        self.cwd_state
            .take_pending_workdir_result(&unloaded.agent_id);
        self.start_agent_owners
            .retain(|_, agent_id| agent_id != &unloaded.agent_id);
    }

    fn handle_agent_metadata_set(&mut self, set: tau_proto::AgentMetadataSet, is_replay: bool) {
        if set.key != self.cwd_state.key() {
            return;
        }
        if self.cwd_state.is_replay_failed(&set.agent_id) {
            return;
        }
        if let CborValue::Text(path) = set.value {
            self.handle_text_cwd_metadata_set(
                set.agent_id,
                PathBuf::from(path),
                set.mutation_id.as_ref(),
                is_replay,
            );
        } else {
            self.handle_invalid_cwd_metadata_set(set.agent_id, set.mutation_id.as_ref(), is_replay);
        }
    }

    fn handle_text_cwd_metadata_set(
        &mut self,
        agent_id: tau_proto::AgentId,
        cwd: PathBuf,
        mutation_id: Option<&tau_proto::AgentMetadataMutationId>,
        is_replay: bool,
    ) {
        if !self
            .cwd_state
            .set_metadata_text(agent_id.clone(), cwd.clone())
        {
            self.handle_invalid_cwd_metadata_set(agent_id, mutation_id, is_replay);
            return;
        }
        if is_replay {
            return;
        }
        if let Some((session_id, initialization_id)) = self.cwd_state.initialization(&agent_id) {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = self
                .tx
                .send(HarnessInputMessage::emit_transient(cwd_context_event(
                    session_id,
                    agent_id.clone(),
                    initialization_id,
                    &cwd,
                    &self.cwd_state,
                )));
        }
        let pending_workdir =
            self.cwd_state
                .take_committed_pending_workdir_result(&agent_id, &cwd, mutation_id);
        if pending_workdir.is_some() {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = self.tx.send(HarnessInputMessage::emit(cwd_notice_event(
                agent_id.clone(),
                &cwd,
            )));
        }
        self.complete_pending_workdir_after_text_metadata(pending_workdir, &cwd);
        self.publish_ready_if_pending(agent_id);
    }

    fn complete_pending_workdir_after_text_metadata(
        &self,
        pending_workdir: Option<crate::cwd_state::CompletedPendingWorkdir>,
        cwd: &Path,
    ) {
        if let Some(pending_workdir) = pending_workdir {
            let event = if pending_workdir.cancel_requested {
                Event::ToolCancelled(tau_proto::ToolCancelled {
                    call_id: pending_workdir.invoke.call_id,
                    tool_name: pending_workdir.invoke.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                })
            } else if pending_workdir.matched_request {
                let output = crate::tools::workdir::output(cwd);
                Event::ToolResult(ToolResult {
                    call_id: pending_workdir.invoke.call_id,
                    tool_name: pending_workdir.invoke.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                    result: output.result,
                    provider_content: Vec::new(),
                    kind: ToolResultKind::Final,
                    display: Some(output.display),
                    originator: pending_workdir.invoke.originator,
                })
            } else {
                Event::ToolError(tau_proto::ToolError {
                    call_id: pending_workdir.invoke.call_id,
                    tool_name: pending_workdir.invoke.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                    message: format!(
                        "committed cwd metadata did not match requested cwd; cwd changed to {}",
                        cwd.display()
                    ),
                    details: None,
                    display: None,
                    originator: pending_workdir.invoke.originator,
                })
            };
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = self.tx.report_tool_terminal(with_lock_wait_duration(
                event,
                pending_workdir.lock_wait_duration_seconds,
            ));
        }
    }

    fn handle_invalid_cwd_metadata_set(
        &mut self,
        agent_id: tau_proto::AgentId,
        mutation_id: Option<&tau_proto::AgentMetadataMutationId>,
        is_replay: bool,
    ) {
        self.cwd_state.set_invalid(agent_id.clone());
        if is_replay {
            return;
        }
        if let Some((session_id, initialization_id)) = self.cwd_state.initialization(&agent_id) {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = self.tx.send(HarnessInputMessage::emit_transient(
                invalid_cwd_context_event(
                    session_id,
                    agent_id.clone(),
                    initialization_id,
                    &self.cwd_state,
                ),
            ));
        }
        if let Some(pending) = self
            .cwd_state
            .take_correlated_pending_workdir_result(&agent_id, mutation_id)
        {
            self.send_pending_workdir_error(
                pending,
                "committed workdir metadata is malformed; workdir setter was superseded",
            );
        }
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
        if self.cwd_state.is_replay_failed(&unset.agent_id) {
            return;
        }
        self.cwd_state.unset(&unset.agent_id);
        if is_replay {
            return;
        }
        if let Ok(cwd) = self.cwd_state.process_default() {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = self.tx.send(HarnessInputMessage::emit_transient(
                Event::AgentMetadataSetRequest(tau_proto::AgentMetadataSet {
                    agent_id: unset.agent_id,
                    key: self.cwd_state.key(),
                    value: CborValue::Text(cwd.display().to_string()),
                    mutation_id: None,
                    inheritable: true,
                }),
            ));
        }
    }

    fn send_pending_workdir_error(
        &self,
        pending: crate::cwd_state::CompletedPendingWorkdir,
        message: &str,
    ) {
        let event = if pending.cancel_requested {
            Event::ToolCancelled(tau_proto::ToolCancelled {
                call_id: pending.invoke.call_id,
                tool_name: pending.invoke.tool_name,
                tool_type: tau_proto::ToolType::Function,
            })
        } else {
            Event::ToolError(tau_proto::ToolError {
                call_id: pending.invoke.call_id,
                tool_name: pending.invoke.tool_name,
                tool_type: tau_proto::ToolType::Function,
                message: message.to_owned(),
                details: None,
                display: None,
                originator: pending.invoke.originator,
            })
        };
        // This call is intentionally best-effort; preserve the existing discarded
        // result. ast-grep-ignore: let-underscore-call
        let _ = self.tx.report_tool_terminal(event);
    }

    fn publish_ready_if_pending(&self, agent_id: tau_proto::AgentId) {
        if let Some((session_id, agent_initialization_id)) =
            self.cwd_state.take_pending_ready(&agent_id)
        {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = self.tx.send(HarnessInputMessage::emit_transient(
                Event::ExtensionContextReady(ExtensionContextReady {
                    session_id,
                    agent_id,
                    agent_initialization_id,
                }),
            ));
        }
    }

    fn handle_agent_replay_complete(&self, done: tau_proto::AgentReplayComplete) {
        let Some((session_id, initialization_id)) = self.cwd_state.pending_ready(&done.agent_id)
        else {
            return;
        };
        publish_agent_discovery_snapshot_for(
            session_id.clone(),
            done.agent_id.clone(),
            initialization_id.clone(),
            &self.tx,
        );
        if done.error.is_some() {
            self.cwd_state.take_pending_ready(&done.agent_id);
            self.cwd_state.set_replay_failed(done.agent_id);
            return;
        }
        if let Some(cwd) = self.cwd_state.get(&done.agent_id) {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = self
                .tx
                .send(HarnessInputMessage::emit_transient(cwd_context_event(
                    session_id.clone(),
                    done.agent_id.clone(),
                    initialization_id.clone(),
                    &cwd,
                    &self.cwd_state,
                )));
            self.publish_ready_if_pending(done.agent_id);
            return;
        }
        if self.cwd_state.is_invalid(&done.agent_id) {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = self.tx.send(HarnessInputMessage::emit_transient(
                invalid_cwd_context_event(
                    session_id.clone(),
                    done.agent_id.clone(),
                    initialization_id.clone(),
                    &self.cwd_state,
                ),
            ));
            self.publish_ready_if_pending(done.agent_id);
            return;
        }
        let Ok(cwd) = self.cwd_state.process_default() else {
            self.cwd_state.take_pending_ready(&done.agent_id);
            return;
        };
        // This call is intentionally best-effort; preserve the existing discarded
        // result. ast-grep-ignore: let-underscore-call
        let _ = self.tx.send(HarnessInputMessage::emit_transient(
            Event::AgentMetadataSetRequest(tau_proto::AgentMetadataSet {
                agent_id: done.agent_id.clone(),
                key: self.cwd_state.key(),
                value: CborValue::Text(cwd.display().to_string()),
                mutation_id: None,
                inheritable: true,
            }),
        ));
        self.cwd_state
            .set_pending_ready(done.agent_id, session_id, initialization_id);
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
            self.cwd_state
                .take_pending_workdir_by_call(&request.target_call_id);
            debug!(call_id = %request.target_call_id, "cancellation requested for queued shell work");
            return;
        }
        if self
            .cwd_state
            .request_pending_workdir_cancel(&request.target_call_id)
        {
            debug!(call_id = %request.target_call_id, "workdir cancellation deferred to metadata commit");
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
        if cmd
            .target_agent_id
            .as_ref()
            .is_some_and(|agent_id| self.cwd_state.pending_ready(agent_id).is_some())
        {
            send_ui_shell_saturated_failure(
                cmd,
                "workdir replay is not complete for the target agent".to_owned(),
                &self.tx,
            );
            return Ok(());
        }
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: match-option-verbose
        let cwd = match cmd.target_agent_id.as_ref() {
            Some(agent_id) => self.cwd_state.get_or_default(agent_id),
            None => self.cwd_state.process_default(),
        };
        let cwd = match cwd {
            Ok(cwd) => cwd,
            Err(message) => {
                send_ui_shell_saturated_failure(cmd, message, &self.tx);
                return Ok(());
            }
        };
        if let Err(error) = schedule_ui_shell_command(
            cmd,
            UiShellScheduleContext {
                scheduler: self.scheduler()?,
                tx: &self.tx,
                shell_config: self.config.shell.clone(),
                running_ui_commands: Arc::clone(&self.running_ui_commands),
                shutdown_generation: Arc::clone(&self.shutdown_generation),
                scheduled_generation: self.shutdown_generation.load(Ordering::SeqCst),
                cwd,
            },
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
                    mutation_id: None,
                    inheritable: true,
                }),
                true,
            )
            .expect("replay metadata");
        assert!(rx.try_recv().is_err(), "replay fold must not emit output");

        runtime
            .handle_event(
                Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                    agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                        .expect("test identifier must be valid"),

                    session_id: "session-1"
                        .parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
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
                    session_id: Some(
                        "session-1"
                            .parse::<tau_proto::SessionId>()
                            .expect("known-safe SessionId must be valid"),
                    ),
                    error: None,
                }),
                false,
            )
            .expect("replay boundary");

        loop {
            let message = rx.recv().expect("discovery snapshot");
            if matches!(
                message,
                HarnessInputMessage::Emit(ref emit)
                    if matches!(emit.event.as_ref(),
                        Event::ExtensionAgentDiscoverySnapshotDeclared(declared)
                            if declared.agent_id == agent_id)
            ) {
                break;
            }
        }
        let HarnessInputMessage::Emit(context) = rx.recv().expect("context publish") else {
            panic!("expected context publish");
        };
        assert!(!context.persist);
        assert!(matches!(
            context.event.as_ref(),
            Event::ExtAgentContextPublish(publish)
                if publish.agent_id == agent_id && publish.key.as_ref() == "workdir"
        ));
        let HarnessInputMessage::Emit(ready) = rx.recv().expect("context ready") else {
            panic!("expected context ready");
        };
        assert!(!ready.persist);
        assert!(matches!(
            ready.event.as_ref(),
            Event::ExtensionContextReady(ready)
                if ready.agent_id == agent_id && ready.session_id == "session-1"
        ));
    }

    /// Malformed restored workdir metadata is present state, not an absent key
    /// that may be overwritten by the process-startup fallback.
    #[test]
    fn malformed_replayed_workdir_is_retained_without_default_seeding() {
        let (tx, rx) = mpsc::channel();
        let mut runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
        let agent_id = tau_proto::AgentId::parse("agent-invalid-workdir").expect("agent id");
        runtime
            .handle_event(
                Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                    agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                        .expect("test identifier must be valid"),

                    session_id: "session-1"
                        .parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                    agent_id: agent_id.clone(),
                    ephemeral: false,
                }),
                false,
            )
            .expect("load");
        runtime
            .handle_event(
                Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                    agent_id: agent_id.clone(),
                    key: runtime.cwd_state.key(),
                    value: CborValue::Bool(true),
                    mutation_id: None,
                    inheritable: true,
                }),
                true,
            )
            .expect("replay malformed metadata");
        runtime
            .handle_event(
                Event::AgentReplayComplete(tau_proto::AgentReplayComplete {
                    agent_id: agent_id.clone(),
                    session_id: Some(
                        "session-1"
                            .parse::<tau_proto::SessionId>()
                            .expect("known-safe SessionId must be valid"),
                    ),
                    error: None,
                }),
                false,
            )
            .expect("complete replay");

        loop {
            let message = rx.recv().expect("discovery snapshot");
            if matches!(
                message,
                HarnessInputMessage::Emit(ref emit)
                    if matches!(emit.event.as_ref(),
                        Event::ExtensionAgentDiscoverySnapshotDeclared(_))
            ) {
                break;
            }
        }
        let first = rx.recv().expect("invalid context");
        assert!(matches!(
            first,
            HarnessInputMessage::Emit(emit)
                if matches!(emit.event.as_ref(), Event::ExtAgentContextPublish(publish)
                    if publish.key.as_ref() == "workdir"
                        && publish.value.0["status"] == "invalid")
                    && !emit.persist
        ));
        let second = rx.recv().expect("ready");
        assert!(matches!(
            second,
            HarnessInputMessage::Emit(emit)
                if matches!(emit.event.as_ref(), Event::ExtensionContextReady(_))
                    && !emit.persist
        ));
        assert!(
            rx.try_recv().is_err(),
            "malformed replay must not synthesize default metadata"
        );
    }

    /// Invalid remembered metadata fails one user shell command without killing
    /// the extension runtime needed for an absolute workdir repair.
    #[test]
    fn invalid_workdir_user_shell_failure_is_command_local() {
        let (tx, rx) = mpsc::channel();
        let runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
        let agent_id = tau_proto::AgentId::parse("agent-invalid-user-shell").expect("agent id");
        runtime.cwd_state.set_invalid(agent_id.clone());
        runtime
            .handle_ui_shell_command(tau_proto::UiShellCommand {
                session_id: "session-1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                command_id: tau_proto::ShellCommandId::parse("command-1")
                    .expect("test identifier must satisfy its grammar"),
                command: "pwd".to_owned(),
                include_in_context: false,
                target_agent_id: Some(agent_id),
            })
            .expect("command-local failure");
        let event = rx.recv().expect("terminal user shell failure");
        assert!(matches!(
            event,
            HarnessInputMessage::Emit(emit)
                if matches!(emit.event.as_ref(), Event::ShellCommandFinishedReported(finished)
                    if finished.command_id.as_str() == "command-1"
                        && finished.output.contains("invalid"))
        ));
        assert!(runtime.scheduler.is_some(), "runtime must remain usable");
    }

    /// User shell work must not use process fallback while durable workdir
    /// replay is still establishing whether the instance key is present.
    #[test]
    fn user_shell_before_workdir_replay_fails_without_spawning() {
        let (tx, rx) = mpsc::channel();
        let runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
        let agent_id = tau_proto::AgentId::parse("agent-replay-pending-shell").expect("agent id");
        runtime.cwd_state.set_pending_ready(
            agent_id.clone(),
            "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            tau_proto::AgentInitializationId::parse("init-1")
                .expect("test identifier must be valid"),
        );
        runtime
            .handle_ui_shell_command(tau_proto::UiShellCommand {
                session_id: "session-1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                command_id: tau_proto::ShellCommandId::parse("command-pending")
                    .expect("test identifier must satisfy its grammar"),
                command: "touch must-not-exist".to_owned(),
                include_in_context: false,
                target_agent_id: Some(agent_id),
            })
            .expect("command-local failure");
        let event = rx.recv().expect("terminal failure");
        assert!(matches!(
            event,
            HarnessInputMessage::Emit(emit)
                if matches!(emit.event.as_ref(), Event::ShellCommandFinishedReported(finished)
                    if finished.output.contains("replay is not complete"))
        ));
    }

    /// Runtime shutdown clears setters awaiting lifecycle completion; the
    /// harness owns terminalizing calls when the extension/session ends.
    #[test]
    fn shutdown_clears_reserved_workdir_setter_without_extension_terminal() {
        let (tx, rx) = mpsc::channel();
        let mut runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
        let agent_id = tau_proto::AgentId::parse("agent-reserved-setter").expect("agent id");
        let invoke = tau_proto::ToolStarted {
            call_id: tau_proto::ToolCallId::new("reserved-setter"),
            tool_name: tau_proto::ToolName::new(crate::tools::WORKDIR_TOOL_NAME),
            arguments: CborValue::Map(Vec::new()),
            agent_id: agent_id.clone(),
            originator: tau_proto::PromptOriginator::User,
        };
        runtime
            .cwd_state
            .start_pending_workdir_result(agent_id, PathBuf::from("/tmp"), invoke, None)
            .expect("reserve setter");
        runtime.shutdown();
        assert!(rx.try_recv().is_err());
        assert!(
            runtime
                .cwd_state
                .take_pending_workdir_by_call(&tau_proto::ToolCallId::new("reserved-setter"))
                .is_none()
        );
    }

    /// The non-interleavable pre-emission state and unrelated commits cannot
    /// consume a setter; only its matching canonical echo reaches the terminal
    /// boundary.
    #[test]
    fn workdir_reservation_commit_phase_is_linearized() {
        let (tx, _rx) = mpsc::channel();
        let runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
        let agent_id = tau_proto::AgentId::parse("agent-linearized-setter").expect("agent id");
        let invoke = tau_proto::ToolStarted {
            call_id: tau_proto::ToolCallId::new("x".repeat(1024)),
            tool_name: tau_proto::ToolName::new(crate::tools::WORKDIR_TOOL_NAME),
            arguments: CborValue::Map(Vec::new()),
            agent_id: agent_id.clone(),
            originator: tau_proto::PromptOriginator::User,
        };
        let expected = PathBuf::from("/expected");
        runtime
            .cwd_state
            .start_pending_workdir_result(agent_id.clone(), expected, invoke.clone(), None)
            .expect("reserve");
        let mutation_id = runtime
            .cwd_state
            .pending_workdir_mutation_id(&agent_id, &invoke.call_id)
            .expect("mutation id");
        assert!(mutation_id.as_str().len() <= tau_proto::MAX_AGENT_METADATA_MUTATION_ID_BYTES);
        assert!(
            runtime
                .cwd_state
                .take_committed_pending_workdir_result(
                    &agent_id,
                    &PathBuf::from("/pre-emission"),
                    Some(&mutation_id),
                )
                .is_none()
        );
        assert!(
            runtime
                .cwd_state
                .mark_pending_workdir_awaiting_echo(&agent_id, &invoke.call_id)
        );
        assert!(
            runtime
                .cwd_state
                .take_committed_pending_workdir_result(
                    &agent_id,
                    &PathBuf::from("/superseding"),
                    None,
                )
                .is_none(),
            "unrelated commit must not consume the setter"
        );
        assert!(
            runtime
                .cwd_state
                .take_committed_pending_workdir_result(
                    &agent_id,
                    &PathBuf::from("/expected"),
                    None,
                )
                .is_none(),
            "same-value external commit must not impersonate the setter echo"
        );
        let completed = runtime
            .cwd_state
            .take_committed_pending_workdir_result(
                &agent_id,
                &PathBuf::from("/expected"),
                Some(&mutation_id),
            )
            .expect("matching echo consumes setter");
        assert!(completed.matched_request);
    }

    /// Awaiting-echo cancellation stays attached to the transaction and emits
    /// exactly one cancellation when its correlated commit arrives.
    #[test]
    fn awaiting_workdir_cancel_terminalizes_at_correlated_commit() {
        let (tx, rx) = mpsc::channel();
        let mut runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
        let agent_id = tau_proto::AgentId::parse("agent-cancel-setter").expect("agent id");
        let invoke = tau_proto::ToolStarted {
            call_id: tau_proto::ToolCallId::new("cancel-setter"),
            tool_name: tau_proto::ToolName::new(crate::tools::WORKDIR_TOOL_NAME),
            arguments: CborValue::Map(Vec::new()),
            agent_id: agent_id.clone(),
            originator: tau_proto::PromptOriginator::User,
        };
        runtime
            .cwd_state
            .start_pending_workdir_result(
                agent_id.clone(),
                PathBuf::from("/tmp"),
                invoke.clone(),
                None,
            )
            .expect("reserve");
        let mutation_id = runtime
            .cwd_state
            .pending_workdir_mutation_id(&agent_id, &invoke.call_id)
            .expect("mutation id");
        assert!(
            runtime
                .cwd_state
                .mark_pending_workdir_awaiting_echo(&agent_id, &invoke.call_id)
        );
        runtime.handle_tool_cancel_request(tau_proto::ToolCancelRequest {
            target_call_id: invoke.call_id.clone(),
        });
        runtime.handle_agent_metadata_set(
            tau_proto::AgentMetadataSet {
                agent_id,
                key: runtime.cwd_state.key(),
                value: CborValue::Text("/tmp".to_owned()),
                mutation_id: Some(mutation_id),
                inheritable: true,
            },
            false,
        );
        let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|message| matches!(
                    message,
                    HarnessInputMessage::Emit(emit)
                        if !emit.persist
                            && matches!(emit.event.as_ref(), Event::ToolCancelledReported(cancelled)
                            if cancelled.call_id.as_str() == "cancel-setter")
                ))
                .count(),
            1
        );
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
                    session_id: "session-1"
                        .parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                }),
                false,
            )
            .expect("session shutdown");

        assert!(runtime.scheduler.is_some());
    }
}
