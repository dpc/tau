//! Runtime state and reader-loop dispatch after the ext-shell handshake.

use std::collections::HashMap;
use std::error::Error;
use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use tau_proto::{
    CborValue, ConfigError, Event, ExtensionContextReady, HarnessInputMessage,
    HarnessOutputMessage, PeerInputReader, ToolResult, ToolResultKind,
};
use tracing::debug;

use super::{
    apply_started_cwd_metadata, apply_working_directory, cwd_context_event, cwd_notice_event,
    dir_lock_tool_spec, dispatch_action_invoke, dispatch_session_agent_loaded,
    dispatch_session_started, is_shell_tool, schedule_tool_started, schedule_ui_shell_command,
    send_tool_failure, send_ui_shell_saturated_failure, with_lock_wait_duration,
};
use crate::config::ExtConfig;
use crate::cwd_state::CwdState;
use crate::dir_lock::DirLockManager;
use crate::scheduler::WorkScheduler;

pub(super) struct ShellRuntime {
    config: ExtConfig,
    scheduler: WorkScheduler,
    tx: mpsc::Sender<HarnessInputMessage>,
    running_calls: Arc<Mutex<HashMap<tau_proto::ToolCallId, mpsc::Sender<()>>>>,
    lock_manager: DirLockManager,
    cwd_state: CwdState,
    start_agent_owners: HashMap<String, tau_proto::AgentId>,
    runtime_started: bool,
}

impl ShellRuntime {
    pub(super) fn new(tx: mpsc::Sender<HarnessInputMessage>, config: ExtConfig) -> Self {
        Self {
            config,
            scheduler: WorkScheduler::new(tx.clone(), Default::default()),
            tx,
            running_calls: Arc::new(Mutex::new(HashMap::new())),
            lock_manager: DirLockManager::default(),
            cwd_state: CwdState::new(),
            start_agent_owners: HashMap::new(),
            runtime_started: false,
        }
    }

    fn send(&self, message: HarnessInputMessage) -> Result<(), Box<dyn Error>> {
        self.tx.send(message).map_err(|error| {
            Box::new(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                format!("response channel closed: {error}"),
            )) as Box<dyn Error>
        })
    }

    pub(super) fn shutdown(&mut self) {
        // Dir-lock waiters must be woken before the scheduler is dropped,
        // because scheduler drop joins workers that may be blocked on locks.
        self.lock_manager.shutdown();
        self.scheduler.cancel_all_queued();
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
    }

    fn handle_configure(&mut self, msg: tau_proto::Configure) -> Result<(), Box<dyn Error>> {
        match tau_extension::parse_config::<ExtConfig>(&msg.config) {
            Ok(cfg) => self.apply_config(msg.instance_name, cfg),
            Err(message) => self.send(HarnessInputMessage::ConfigError(ConfigError { message })),
        }
    }

    fn apply_config(
        &mut self,
        instance_name: Option<tau_proto::ExtensionName>,
        mut cfg: ExtConfig,
    ) -> Result<(), Box<dyn Error>> {
        if cfg.working_directory.is_none() {
            cfg.working_directory = self.config.working_directory.clone();
        }
        if let Some(instance_name) = instance_name.as_ref() {
            self.cwd_state
                .set_instance_name(instance_name.as_str().to_owned());
        }
        if let Err(message) = apply_working_directory(&self.config, &cfg, self.runtime_started) {
            return self.send(HarnessInputMessage::ConfigError(ConfigError { message }));
        }
        if let Err(message) = self.lock_manager.configure(&cfg.dir_lock) {
            return self.send(HarnessInputMessage::ConfigError(ConfigError { message }));
        }

        let dir_lock_was_enabled = self.config.dir_lock.enable;
        let dir_lock_changed = dir_lock_was_enabled != cfg.dir_lock.enable;
        let dir_lock_disabling = dir_lock_was_enabled && !cfg.dir_lock.enable;
        self.config = cfg;
        if dir_lock_disabling {
            let _ = self.lock_manager.disable();
        }
        if dir_lock_changed {
            self.send(HarnessInputMessage::emit(Event::ToolRegister(
                tau_proto::ToolRegister {
                    tool: dir_lock_tool_spec(self.config.dir_lock.enable),
                    tool_group: Some(tau_proto::ToolGroup {
                        name: tau_proto::ToolGroupName::new("shell"),
                        prompt_fragment: None,
                    }),
                    prompt_fragment: None,
                },
            )))?;
        }
        Ok(())
    }

    fn handle_delivery(
        &mut self,
        delivery: tau_proto::EventDelivery,
    ) -> Result<(), Box<dyn Error>> {
        self.runtime_started = true;
        // Replay-marked frames re-send historical facts to late subscribers.
        // Execution triggers are skipped on replay so history does not re-run
        // side effects; metadata-bearing facts are folded so cwd state is
        // restored before live readiness.
        let is_replay = delivery.is_replay();
        self.handle_event(delivery.into_event(), is_replay)
    }

    fn handle_event(&mut self, event: Event, is_replay: bool) -> Result<(), Box<dyn Error>> {
        match event {
            Event::AgentStarted(started) => {
                apply_started_cwd_metadata(started, &self.tx, &self.cwd_state, is_replay);
            }
            Event::ToolStarted(invoke) => {
                self.handle_tool_started(invoke, is_replay);
            }
            Event::SessionStarted(started) => {
                if !is_replay {
                    dispatch_session_started(started, &self.tx);
                }
            }
            Event::SessionAgentLoaded(loaded) => {
                if !is_replay {
                    dispatch_session_agent_loaded(loaded, &self.tx, &self.cwd_state);
                }
            }
            Event::SessionAgentUnloaded(unloaded) => {
                if !is_replay {
                    self.handle_session_agent_unloaded(unloaded);
                }
            }
            Event::AgentMetadataSet(set) => self.handle_agent_metadata_set(set, is_replay),
            Event::AgentMetadataUnset(unset) => self.handle_agent_metadata_unset(unset, is_replay),
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
            Event::UiShellCommand(cmd) => self.handle_ui_shell_command(cmd),
            _ => {}
        }
        Ok(())
    }

    fn handle_tool_started(&self, invoke: tau_proto::ToolStarted, is_replay: bool) {
        if is_replay || !is_shell_tool(invoke.tool_name.as_str()) {
            return;
        }
        if let Err(error) = schedule_tool_started(
            invoke,
            &self.scheduler,
            &self.tx,
            self.config.clone(),
            self.lock_manager.clone(),
            Arc::clone(&self.running_calls),
            self.cwd_state.clone(),
        ) {
            let (invoke, failure) = *error;
            send_tool_failure(invoke, failure, &self.tx);
        }
    }

    fn handle_session_agent_unloaded(&mut self, unloaded: tau_proto::SessionAgentUnloaded) {
        self.lock_manager.release_agent(&unloaded.agent_id);
        self.scheduler.cancel_agent(&unloaded.agent_id);
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

    fn shutdown_session(&mut self) {
        self.shutdown();
        self.start_agent_owners.clear();
    }

    fn handle_start_agent_result(&mut self, result: tau_proto::StartAgentResult) {
        if let Some(agent_id) = self.start_agent_owners.remove(&result.query_id) {
            self.lock_manager.release_agent(&agent_id);
            self.scheduler.cancel_agent(&agent_id);
        }
    }

    fn handle_tool_cancel_request(&self, request: tau_proto::ToolCancelRequest) {
        if self.scheduler.cancel_queued_call(&request.target_call_id) {
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

    fn handle_ui_shell_command(&self, cmd: tau_proto::UiShellCommand) {
        if let Err(error) =
            schedule_ui_shell_command(cmd, &self.scheduler, &self.tx, self.config.shell.clone())
        {
            let (cmd, message) = *error;
            send_ui_shell_saturated_failure(cmd, message, &self.tx);
        }
    }

    pub(super) fn run_reader_loop<R: Read>(
        &mut self,
        reader: &mut PeerInputReader<BufReader<R>>,
    ) -> Result<(), Box<dyn Error>> {
        // Reader loop: dispatch each owned tool invocation to a worker thread.
        // ToolStarted is a subscribed committed delivery, so it carries an ack
        // sequence that must be acknowledged after processing like other subscribed
        // events.
        loop {
            match reader.read_message()? {
                Some(HarnessOutputMessage::Configure(msg)) => self.handle_configure(msg)?,
                Some(HarnessOutputMessage::Deliver(delivery)) => self.handle_delivery(delivery)?,
                Some(HarnessOutputMessage::Disconnect(_)) | None => return Ok(()),
                Some(_) => {}
            }
        }
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
        let runtime = ShellRuntime::new(tx, ExtConfig::default());
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
        let mut runtime = ShellRuntime::new(tx, ExtConfig::default());
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
}
