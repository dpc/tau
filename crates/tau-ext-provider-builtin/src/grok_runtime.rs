//! Main-loop Secret mediation for process-local Grok refresh flights.

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tau_client::ManualRuntimeWaker;
use tau_proto::{
    ExtensionDataErrorKind, ExtensionDataPath, ExtensionDataRequestOp, ExtensionDataResultPayload,
    ExtensionDataScope, ExtensionDataValue, ProviderName,
};
use tau_provider_grok::credential::{self, Credential, RefreshReason, Storage, StorageError};
use tau_provider_grok::oauth::Client;
use tokio::runtime::Builder;
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::{
    BuiltinProviderProfile, BuiltinProviderProfiles, CredentialObservation, ProviderCredential,
    ProviderRuntime, WorkerMessage, backend_profile_identity, now_ms, send_worker_message,
};

/// Exact Secret generation; no bearer or subject appears in map diagnostics.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(crate) struct Key {
    /// Selected namespace.
    provider: ProviderName,
    /// Closed selected Secret path.
    path: ExtensionDataPath,
    /// Hash of the exact bytes read.
    generation: String,
    /// Frozen public registration.
    client_id: String,
}

/// One retained worker, even when all inference waiters disappear.
struct Flight {
    /// Every joining waiter must reject an unchanged unauthorized bearer.
    forced: bool,
    /// Cancellation is checked before exchange, never during publication.
    canceled: Arc<AtomicBool>,
}

/// Private runtime ownership, independent of ChatGPT's refresh implementation.
#[derive(Default)]
pub(crate) struct State {
    /// Same-generation admissions share one remote exchange.
    flights: HashMap<Key, Flight>,
    /// Failed observed generations must not reuse possibly consumed tokens.
    failed: HashSet<Key>,
    /// Outstanding replies are removed on completion, including worker timeout.
    rpcs: HashMap<String, Rpc>,
}

/// One main-loop mediated read or CAS; never serialized outside Secret RPC.
pub(crate) struct Rpc {
    /// Exact owner generation.
    key: Key,
    /// Existing closed operation, restricted to the selected path by the
    /// worker.
    op: ExtensionDataRequestOp,
    /// Single bounded reply consumed by the retained worker.
    reply: oneshot::Sender<ExtensionDataResultPayload>,
}

/// Worker-side callbacks; the non-Send extension client stays on the main loop.
struct Store {
    /// Selected authority.
    key: Key,
    /// Main-loop mailbox.
    tx: Sender<WorkerMessage>,
    /// Notification-driven wakeup.
    waker: ManualRuntimeWaker,
    /// First authoritative read used by resolve, never its later CAS readback.
    observed_generation: Mutex<Option<String>>,
    /// Exact latest successful read, distinct from the exchanged generation.
    authoritative_generation: Mutex<Option<blake3::Hash>>,
    /// Changed authoritative bytes are returned to the main loop for restaging,
    /// never exchanged under another generation's flight.
    changed_generation: Mutex<Option<Vec<u8>>>,
}

impl Store {
    /// Turn a pre-exchange handoff into private main-loop work, preserving
    /// validated account binding without changing the credential library API.
    fn completion(
        &self,
        observed: &Credential,
        mut result: Result<Credential, credential::Error>,
    ) -> WorkerMessage {
        let mut observed_generation = self
            .observed_generation
            .lock()
            .expect("generation lock")
            .clone();
        if let Some(bytes) = self
            .changed_generation
            .lock()
            .expect("changed generation")
            .take()
        {
            // The guarded changed generation was never exchanged by this
            // worker.
            observed_generation = None;
            match Credential::decode(&bytes) {
                Ok(credential) if credential.subject() == observed.subject() => {
                    return WorkerMessage::GrokGenerationChanged {
                        key: self.key.clone(),
                        generation: blake3::hash(&bytes),
                        credential,
                    };
                }
                Ok(_) => result = Err(credential::Error::AccountChanged),
                Err(error) => result = Err(error),
            }
        }
        WorkerMessage::GrokRefreshFinished {
            key: self.key.clone(),
            observed_generation,
            authoritative_generation: *self
                .authoritative_generation
                .lock()
                .expect("authoritative generation"),
            result,
        }
    }

    /// Submit one existing Secret request and wait with a finite deadline.
    async fn request(
        &self,
        op: ExtensionDataRequestOp,
    ) -> Result<ExtensionDataResultPayload, StorageError> {
        let (reply, receiver) = oneshot::channel();
        send_worker_message(
            &self.tx,
            &self.waker,
            WorkerMessage::GrokSecretRequest(Rpc {
                key: self.key.clone(),
                op,
                reply,
            }),
        )
        .map_err(|_| StorageError::Unavailable)?;
        timeout(Duration::from_secs(30), receiver)
            .await
            .map_err(|_| StorageError::OutcomeUnknown)?
            .map_err(|_| StorageError::Unavailable)
    }
}

impl Storage for Store {
    async fn read(&self) -> Result<Option<Vec<u8>>, StorageError> {
        match self
            .request(ExtensionDataRequestOp::ReadFile {
                path: self.key.path.clone(),
            })
            .await?
        {
            ExtensionDataResultPayload::Ok {
                value: ExtensionDataValue::ReadFile { contents },
            } => {
                let hash = blake3::hash(&contents);
                let generation = hash.to_hex().to_string();
                let mut first = self.observed_generation.lock().expect("generation lock");
                if first.is_none() {
                    *first = Some(generation.clone());
                    if generation != self.key.generation {
                        *self.changed_generation.lock().expect("changed generation") =
                            Some(contents);
                        return Err(StorageError::Unavailable);
                    }
                }
                *self
                    .authoritative_generation
                    .lock()
                    .expect("authoritative generation") = Some(hash);
                Ok(Some(contents))
            }
            ExtensionDataResultPayload::Error {
                kind: ExtensionDataErrorKind::NotFound,
                ..
            } => Ok(None),
            _ => Err(StorageError::Unavailable),
        }
    }

    async fn compare_and_swap(
        &self,
        expected: &[u8],
        replacement: &[u8],
    ) -> Result<bool, StorageError> {
        match self
            .request(ExtensionDataRequestOp::CompareAndSwapFile {
                path: self.key.path.clone(),
                expected_generation: blake3::hash(expected).to_hex().to_string(),
                contents: replacement.to_vec(),
            })
            .await?
        {
            ExtensionDataResultPayload::Ok {
                value: ExtensionDataValue::CompareAndSwapFile,
            } => Ok(true),
            ExtensionDataResultPayload::Error {
                kind: ExtensionDataErrorKind::GenerationMismatch,
                ..
            } => Ok(false),
            _ => Err(StorageError::OutcomeUnknown),
        }
    }
}

impl State {
    /// Orderly EOF retains workers until their bounded publication finishes.
    pub(crate) fn is_idle(&self) -> bool {
        self.flights.is_empty()
    }
    /// Consume only this module's correlated replies; late replies are
    /// harmless.
    pub(crate) fn result(&mut self, request_id: &str, result: &ExtensionDataResultPayload) -> bool {
        let Some(rpc) = self.rpcs.remove(request_id) else {
            return false;
        };
        let _ = rpc.reply.send(result.clone());
        true
    }
}

impl<F> ProviderRuntime<F>
where
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    /// Join or create a local exact-generation flight after authoritative read.
    pub(crate) fn stage_grok_refresh(&mut self, request_id: &str) {
        let Some(index) = self
            .credential_admission
            .admissions
            .iter()
            .position(|admission| admission.request_id.as_deref() == Some(request_id))
        else {
            return;
        };
        self.stage_grok_refresh_for_admission(index);
    }

    /// Restaging keeps already-consumed 401 authority on its original waiter.
    fn stage_grok_refresh_for_admission(&mut self, index: usize) {
        let admission = &self.credential_admission.admissions[index];
        let carried_forced = admission.oauth_forced;
        let provider = admission.kind.model().provider.clone();
        let Some(BuiltinProviderProfile::Grok(profile)) =
            admission.profiles.providers.get(&provider)
        else {
            return;
        };
        let Some(current) = profile.credential.clone() else {
            return;
        };
        let Some(ProviderCredential::Stored(reference)) =
            admission.profiles.credentials.get(&provider)
        else {
            return;
        };
        let Some(CredentialObservation::Contents(generation)) = admission
            .observations
            .as_ref()
            .and_then(|observations| observations.get(&provider))
        else {
            return;
        };
        let key = Key {
            provider: provider.clone(),
            path: reference.path().clone(),
            generation: generation.to_hex().to_string(),
            client_id: profile.client_id.clone(),
        };
        // Expired credentials do not resolve for inference, but their identity
        // still owns unauthorized recovery and rejection bookkeeping.
        let identity = backend_profile_identity(&crate::PromptBackend::Grok {
            profile: Arc::new(profile.clone()),
            model_index: 0,
        });
        let exhausted = identity.is_some_and(|identity| {
            self.oauth_refresh_rejections
                .unauthorized_exhausted(&provider, identity)
        });
        let forced = carried_forced
            || identity.is_some_and(|identity| {
                self.oauth_refresh_rejections
                    .take_unauthorized(&provider, identity)
            });
        let state = &mut self.credential_admission.grok;
        if let Some(flight) = state.flights.get(&key) {
            self.credential_admission.admissions[index].grok_refresh = Some(key);
            self.credential_admission.admissions[index].oauth_forced = forced || flight.forced;
            return;
        }
        if (exhausted && !carried_forced) || state.failed.contains(&key) {
            let admission = &mut self.credential_admission.admissions[index];
            admission.profiles.providers.remove(&provider);
            admission.observations = Some(BTreeMap::from([(
                provider.clone(),
                CredentialObservation::Unavailable,
            )]));
            admission.profiles.missing_logins.insert(provider);
            return;
        }
        if !forced && !current.is_expired(now_ms()) {
            return;
        }
        let canceled = Arc::new(AtomicBool::new(false));
        state.flights.insert(
            key.clone(),
            Flight {
                forced,
                canceled: Arc::clone(&canceled),
            },
        );
        self.credential_admission.admissions[index].grok_refresh = Some(key.clone());
        self.credential_admission.admissions[index].oauth_forced = forced;
        #[cfg(test)]
        if self.diagnostics.receipt.suppress_oauth_worker {
            return;
        }
        let store = Store {
            observed_generation: Mutex::new(None),
            authoritative_generation: Mutex::new(None),
            changed_generation: Mutex::new(None),
            key: key.clone(),
            tx: self.worker_tx.clone(),
            waker: self
                .worker_waker
                .as_ref()
                .expect("runtime waker installed")
                .clone(),
        };
        let runtime = Arc::clone(&self.codex_runtime);
        std::thread::spawn(move || {
            let result = (|| {
                let executor = Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| credential::Error::StorageUnavailable)?;
                let client = Client::new(&key.client_id, runtime.network())
                    .map_err(|_| credential::Error::StorageUnavailable)?;
                executor.block_on(credential::resolve(
                    &store,
                    &client,
                    &current,
                    if forced {
                        RefreshReason::Unauthorized
                    } else {
                        RefreshReason::Expired
                    },
                    now_ms(),
                    || canceled.load(Ordering::Acquire),
                ))
            })();
            let _ =
                send_worker_message(&store.tx, &store.waker, store.completion(&current, result));
        });
    }

    /// Restage a same-account authoritative generation before any exchange.
    /// This joins an existing B flight rather than exchanging B under A's key.
    pub(crate) fn finish_grok_generation_changed(
        &mut self,
        key: Key,
        credential: Credential,
        generation: blake3::Hash,
    ) {
        let state = &mut self.credential_admission.grok;
        if state.flights.remove(&key).is_none() {
            return;
        }
        state.rpcs.retain(|_, rpc| rpc.key != key);
        let mut restage = Vec::new();
        for (index, admission) in self.credential_admission.admissions.iter_mut().enumerate() {
            if admission.grok_refresh.as_ref() != Some(&key) {
                continue;
            }
            if let Some(BuiltinProviderProfile::Grok(profile)) =
                admission.profiles.providers.get_mut(&key.provider)
            {
                // A new bearer already satisfies old-bearer 401 remediation.
                // Unchanged bearer still carries its consumed single authority.
                admission.oauth_forced &= profile
                    .credential
                    .as_ref()
                    .is_some_and(|old| old.access_token() == credential.access_token());
                profile.credential = Some(credential.clone());
                admission.observations = Some(BTreeMap::from([(
                    key.provider.clone(),
                    CredentialObservation::Contents(generation),
                )]));
            }
            admission.grok_refresh = None;
            restage.push(index);
        }
        for index in restage {
            self.stage_grok_refresh_for_admission(index);
        }
    }

    /// Submit from the main loop, preserving FIFO CAS-then-read ordering.
    pub(crate) fn start_grok_secret_request(&mut self, rpc: Rpc) {
        if !self
            .credential_admission
            .grok
            .flights
            .contains_key(&rpc.key)
        {
            return;
        }
        let Some(client) = &self.extension_data_client else {
            return;
        };
        if let Ok(id) = client.start_request(ExtensionDataScope::Secret, rpc.op.clone()) {
            self.credential_admission.grok.rpcs.insert(id, rpc);
        }
    }

    /// Publish only the library's validated, durably acknowledged generation.
    pub(crate) fn finish_grok_refresh(
        &mut self,
        key: Key,
        observed_generation: Option<String>,
        authoritative_generation: Option<blake3::Hash>,
        result: Result<Credential, credential::Error>,
    ) {
        let result = if result.is_ok() && authoritative_generation.is_none() {
            Err(credential::Error::PublicationUnknown)
        } else {
            result
        };
        let state = &mut self.credential_admission.grok;
        if state.flights.remove(&key).is_none() {
            return;
        }
        state.rpcs.retain(|_, rpc| rpc.key != key);
        if matches!(result, Err(credential::Error::Canceled)) {
            let indices = self
                .credential_admission
                .admissions
                .iter_mut()
                .enumerate()
                .filter_map(|(index, admission)| {
                    if admission.grok_refresh.as_ref() != Some(&key) {
                        return None;
                    }
                    admission.grok_refresh = None;
                    Some(index)
                })
                .collect::<Vec<_>>();
            for index in indices {
                self.stage_grok_refresh_for_admission(index);
            }
            return;
        }
        if result.is_err() {
            state.failed.insert(key.clone());
            if let Some(generation) = observed_generation {
                state.failed.insert(Key {
                    generation,
                    ..key.clone()
                });
            }
        }
        // Omitted expires_in preserves metadata, not permission to reuse an
        // expired replacement's refresh token on every subsequent admission.
        if let (Ok(next), Some(generation)) = (&result, authoritative_generation)
            && next.is_expired(now_ms())
        {
            state.failed.insert(Key {
                generation: generation.to_hex().to_string(),
                ..key.clone()
            });
        }
        for admission in &mut self.credential_admission.admissions {
            if admission.grok_refresh.as_ref() != Some(&key) {
                continue;
            }
            if let Some(BuiltinProviderProfile::Grok(profile)) =
                admission.profiles.providers.get_mut(&key.provider)
            {
                let authoritative = result.as_ref().ok().filter(|next| {
                    !next.is_expired(now_ms())
                        && (!admission.oauth_forced
                            || profile.credential.as_ref().is_some_and(|previous| {
                                previous.access_token() != next.access_token()
                            }))
                });
                profile.credential = authoritative.cloned();
                if authoritative.is_none() {
                    admission
                        .profiles
                        .missing_logins
                        .insert(key.provider.clone());
                }
                if authoritative.is_some()
                    && let Some(generation) = authoritative_generation
                {
                    admission.observations = Some(BTreeMap::from([(
                        key.provider.clone(),
                        CredentialObservation::Contents(generation),
                    )]));
                }
            }
            if admission.profiles.missing_login(&key.provider) {
                admission.profiles.providers.remove(&key.provider);
                admission.observations = Some(BTreeMap::from([(
                    key.provider.clone(),
                    CredentialObservation::Unavailable,
                )]));
            }
            admission.grok_refresh = None;
        }
    }

    /// A canceled waiter cannot abandon a worker that may hold rotated tokens.
    pub(crate) fn cancel_unobserved_grok_refreshes(&self) {
        for (key, flight) in &self.credential_admission.grok.flights {
            if !self
                .credential_admission
                .admissions
                .iter()
                .any(|admission| admission.grok_refresh.as_ref() == Some(key))
            {
                flight.canceled.store(true, Ordering::Release);
            }
        }
    }
}
