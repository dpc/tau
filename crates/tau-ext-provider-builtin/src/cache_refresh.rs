//! Receipt-relative admission and terminal ownership for optional cache
//! refreshes.

use std::collections::BTreeMap;
use std::thread;
use std::time::{Duration, Instant};

use tau_client::{ClientHandle, ClientResult};
use tau_config::provider_settings::ProviderCredential;
use tau_proto::{ModelId, ProviderName};

use crate::{
    BuiltinProviderProfiles, PendingPromptAdmission, PendingPromptAdmissionKind, PrewarmExecution,
    PrewarmKey, PromptBackend, ProviderRuntime, WorkerMessage, backend_profile_identity,
    debug_provider_requests_for, resolve_prompt_backend_without_refresh,
    send_cache_refresh_terminal, send_worker_message,
};

impl<F> ProviderRuntime<F>
where
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    /// Captures receipt authority before starting any asynchronous credentials.
    pub(super) fn cache_refresh_backend(
        &mut self,
        refresh: tau_proto::AgentCacheRefreshRequested,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let deadline =
            Instant::now() + Duration::from_millis(u64::from(refresh.stop_after_millis.get()));
        let Some(model) = refresh.prompt.model.clone() else {
            return send_cache_refresh_terminal(
                handle,
                refresh.refresh_id,
                tau_proto::ProviderCacheRefreshStatus::Unsupported,
            );
        };
        let mut profiles = (self.load_prompt_profiles)(Some(&model.provider));
        profiles.apply_startup_responses_modes(&self.configuration.startup_responses_modes);
        let request_id = if let (Some(ProviderCredential::Stored(reference)), Some(client)) = (
            profiles.credentials.get(&model.provider),
            self.extension_data_client.as_ref(),
        ) {
            Some(client.start_request(
                tau_proto::ExtensionDataScope::Secret,
                tau_proto::ExtensionDataRequestOp::ReadFile {
                    path: reference.path().clone(),
                },
            )?)
        } else {
            None
        };
        if let Some(request_id) = &request_id {
            self.arm_prompt_credential_timeout(request_id.clone());
        }
        if let Some(deadlines) = &self.credential_admission.deadlines {
            deadlines.schedule(format!("cache-refresh:{}", refresh.refresh_id), deadline);
        }
        let ready = request_id.is_none();
        self.credential_admission
            .admissions
            .push_back(PendingPromptAdmission {
                kind: PendingPromptAdmissionKind::CacheRefresh {
                    refresh,
                    model,
                    deadline,
                },
                profiles,
                request_id,
                observations: ready.then(BTreeMap::new),
                oauth_refresh: None,
                oauth_forced: false,
                receipt_observation: None,
            });
        Ok(())
    }

    /// Transfers a ready maintenance admission to its sole worker terminal
    /// owner.
    pub(super) fn start_admitted_cache_refresh(
        &mut self,
        refresh: tau_proto::AgentCacheRefreshRequested,
        model: ModelId,
        deadline: Instant,
        profiles: &mut BuiltinProviderProfiles,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let refresh_id = refresh.refresh_id.clone();
        if deadline <= Instant::now() {
            return send_cache_refresh_terminal(
                handle,
                refresh_id,
                tau_proto::ProviderCacheRefreshStatus::DeadlineExceeded,
            );
        }
        let Some(PromptBackend::Responses(config)) = resolve_prompt_backend_without_refresh(
            &model,
            profiles,
            &mut self.oauth_refresh_rejections,
        ) else {
            return send_cache_refresh_terminal(
                handle,
                refresh_id,
                tau_proto::ProviderCacheRefreshStatus::Unsupported,
            );
        };
        let identity = backend_profile_identity(&PromptBackend::Responses(config.clone()));
        self.reconcile_provider_profile(&model.provider, identity);
        if matches!(
            self.shared_cooldowns.get(&model.provider),
            Some(cooldown) if cooldown.not_before > self.retry_clock.now()
        ) {
            return send_cache_refresh_terminal(
                handle,
                refresh_id,
                tau_proto::ProviderCacheRefreshStatus::Failed,
            );
        }
        self.reconcile_prewarm_profile(&model.provider, &config);
        let key = PrewarmKey {
            provider: model.provider,
            agent_id: refresh.prompt.agent_id.clone(),
            refresh_id: Some(refresh_id.clone()),
        };
        let Some((generation, abort)) = self.prewarm_supervisor.begin(key.clone()) else {
            return send_cache_refresh_terminal(
                handle,
                refresh_id,
                tau_proto::ProviderCacheRefreshStatus::Failed,
            );
        };
        let deadline_abort = abort.clone();
        thread::spawn(move || {
            if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                thread::sleep(remaining);
            }
            deadline_abort.cancel();
        });
        let debug_provider_requests = debug_provider_requests_for(
            &refresh.prompt.session_id,
            &self.diagnostics.session_debug_allowed,
        );
        let executor = self.prewarm_executor.clone();
        let runtime = self.codex_runtime.clone();
        let tx = self.worker_tx.clone();
        let waker = self
            .worker_waker
            .as_ref()
            .expect("provider runtime worker waker is installed before dispatch")
            .clone();
        thread::spawn(move || {
            let status = PrewarmExecution {
                runtime,
                config,
                request: refresh.prompt,
                refresh_id: Some(refresh_id.clone()),
                debug_provider_requests,
                abort,
            }
            .execute_refresh(&executor, deadline, Instant::now);
            let _ = send_worker_message(
                &tx,
                &waker,
                WorkerMessage::PrewarmDone {
                    key,
                    generation,
                    terminal: Some((refresh_id, status)),
                },
            );
        });
        Ok(())
    }

    /// Cancels local timers without revoking another OAuth consumer's
    /// authority.
    pub(super) fn cancel_admission_deadlines(&self, admission: &PendingPromptAdmission) {
        if let Some(deadlines) = &self.credential_admission.deadlines {
            if let Some(id) = &admission.request_id {
                deadlines.cancel(id.clone());
            }
            if let PendingPromptAdmissionKind::CacheRefresh { refresh, .. } = &admission.kind {
                deadlines.cancel(format!("cache-refresh:{}", refresh.refresh_id));
            }
        }
    }

    /// Retires pending maintenance before a later prompt or stale credential
    /// reply.
    pub(super) fn cancel_pending_cache_refresh(
        &mut self,
        id: &tau_proto::ProviderCacheRefreshId,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let index = self
            .credential_admission
            .admissions
            .iter()
            .position(|admission| {
                matches!(&admission.kind, PendingPromptAdmissionKind::CacheRefresh { refresh, .. }
                if &refresh.refresh_id == id)
            });
        if let Some(index) = index {
            let admission = self
                .credential_admission
                .admissions
                .remove(index)
                .expect("located admission");
            self.cancel_admission_deadlines(&admission);
            if let Some(key) = &admission.oauth_refresh {
                self.prune_unreferenced_prompt_oauth(key);
            }
            self.finish_canceled_admission(&admission, handle)?;
        }
        Ok(())
    }

    /// Reports pending maintenance cancellation while shutdown can still write.
    pub(super) fn cancel_pending_cache_refreshes(
        &mut self,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        while let Some(id) = self
            .credential_admission
            .admissions
            .iter()
            .find_map(|admission| match &admission.kind {
                PendingPromptAdmissionKind::CacheRefresh { refresh, .. } => {
                    Some(refresh.refresh_id.clone())
                }
                _ => None,
            })
        {
            self.cancel_pending_cache_refresh(&id, handle)?;
        }
        Ok(())
    }

    /// Enforces the receipt deadline even while Secret or OAuth work is held.
    pub(super) fn expire_pending_cache_refreshes(
        &mut self,
        now: Instant,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        while let Some(index) = self
            .credential_admission
            .admissions
            .iter()
            .position(|admission| {
                matches!(&admission.kind, PendingPromptAdmissionKind::CacheRefresh { deadline, .. }
                if *deadline <= now)
            })
        {
            let admission = self
                .credential_admission
                .admissions
                .remove(index)
                .expect("expired admission");
            self.cancel_admission_deadlines(&admission);
            if let Some(key) = &admission.oauth_refresh {
                self.prune_unreferenced_prompt_oauth(key);
            }
            if let PendingPromptAdmissionKind::CacheRefresh { refresh, .. } = admission.kind {
                send_cache_refresh_terminal(
                    handle,
                    refresh.refresh_id,
                    tau_proto::ProviderCacheRefreshStatus::DeadlineExceeded,
                )?;
            }
        }
        Ok(())
    }
}
