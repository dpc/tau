//! Retained prompt-worker renewal through main-loop Secret RPC and OS locks.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use fs2::FileExt as _;
use tau_proto::{
    ExtensionDataRequestOp, ExtensionDataResultPayload, ExtensionDataScope, ExtensionDataValue,
};
use tau_provider_chatgpt::credential::Credential;
use tau_provider_chatgpt::oauth::Client;
use tokio::runtime::Builder;

#[cfg(test)]
mod tests;

/// Process-local RPC correlation and uncertain-generation suppression.
#[derive(Default)]
pub(crate) struct State {
    /// Pending replies contain no journal or diagnostic authority.
    pub(crate) replies: HashMap<String, mpsc::Sender<ExtensionDataResultPayload>>,
    /// Failed exchange generations may not be automatically reused live.
    pub(crate) failed: Arc<Mutex<HashSet<blake3::Hash>>>,
}

/// Worker-owned bridge bound to one exact Secret path and lock domain.
pub(crate) struct Session {
    /// Instance-local state root, matching the selected instance's Secret
    /// scope.
    root: PathBuf,
    /// Exact closed credential path, shared by all aliases of this
    /// registration.
    path: tau_proto::ExtensionDataPath,
    /// Main-loop mailbox for bounded Secret operations.
    tx: mpsc::Sender<crate::WorkerMessage>,
    /// Wake the main loop when a worker needs credential I/O.
    waker: tau_client::ManualRuntimeWaker,
    /// Process-wide suppression for consumed or ambiguous generations.
    failed: Arc<Mutex<HashSet<blake3::Hash>>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatGptSession(<private>)")
    }
}

/// One private worker-to-main-loop Secret request.
pub(crate) struct Rpc {
    /// Read/CAS operation scoped by the Session, not model input.
    pub(crate) op: ExtensionDataRequestOp,
    /// Reply retained only until its finite waiter completes.
    pub(crate) reply: mpsc::Sender<ExtensionDataResultPayload>,
}

impl Session {
    /// Obtain an authoritative usable generation while serializing every remote
    /// exchange across all processes sharing this credential's Secret scope.
    pub(crate) fn credential(
        &self,
        selected: &Credential,
        network: &tau_provider::OutboundNetworkPolicy,
        canceled: &mut impl FnMut() -> bool,
    ) -> Result<Credential, &'static str> {
        self.credential_with_exchange(selected, canceled, |current| {
            let executor = Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| tau_provider_chatgpt::Error::Transport)?;
            let client = Client::new(network)?;
            executor.block_on(client.refresh(current))
        })
    }

    /// Keep storage, cancellation and publication identical in production and
    /// synthetic exchange-race tests.
    fn credential_with_exchange(
        &self,
        selected: &Credential,
        canceled: &mut impl FnMut() -> bool,
        exchange: impl FnOnce(&Credential) -> Result<Credential, tau_provider_chatgpt::Error>,
    ) -> Result<Credential, &'static str> {
        let _lock = lock(&self.root, &self.path, canceled)?;
        let bytes = self.read()?;
        let current = Credential::decode(&bytes)
            .map_err(|_| "invalid ChatGPT registration; sign in again")?;
        if current.client_id() != selected.client_id() || current.subject() != selected.subject() {
            return Err("ChatGPT account changed; select the account explicitly");
        }
        current
            .access_token()
            .map_err(|_| "ChatGPT plan permission is disabled; sign in with plan usage enabled")?;
        if !current.needs_refresh(crate::now_ms()) {
            return Ok(current);
        }
        if canceled() {
            return Err("ChatGPT credential renewal canceled before exchange");
        }
        let generation = blake3::hash(&bytes);
        if self
            .failed
            .lock()
            .map_err(|_| "credential state unavailable")?
            .contains(&generation)
        {
            return Err("ChatGPT renewal already failed for this token; sign in again");
        }
        // Once exchange begins, cancellation must not abandon a replacement.
        let result = exchange(&current);
        let refreshed = match result {
            Ok(next) => next,
            Err(error) => {
                self.failed
                    .lock()
                    .map_err(|_| "credential state unavailable")?
                    .insert(generation);
                if error == tau_provider_chatgpt::Error::InvalidGrant {
                    let mut signed_out = current;
                    signed_out.sign_out();
                    let _ = self.publish(&bytes, &signed_out);
                    return Err("ChatGPT renewable session is invalid; sign in again");
                }
                return Err(
                    "ChatGPT renewal failed; credentials preserved, sign in again before retrying in this process",
                );
            }
        };
        if self.publish(&bytes, &refreshed).is_err() {
            self.failed
                .lock()
                .map_err(|_| "credential state unavailable")?
                .insert(generation);
            return Err("ChatGPT renewed credentials could not be confirmed saved; sign in again");
        }
        let authoritative = Credential::decode(&self.read()?)
            .map_err(|_| "ChatGPT credential publication was not confirmed")?;
        if authoritative.client_id() != selected.client_id()
            || authoritative.subject() != selected.subject()
            || authoritative.needs_refresh(crate::now_ms())
        {
            return Err("ChatGPT account changed during renewal; select the account explicitly");
        }
        authoritative
            .access_token()
            .map_err(|_| "ChatGPT plan permission is disabled")?;
        Ok(authoritative)
    }

    /// Submit and await one bounded operation; late replies cannot mutate the
    /// worker's account selection or resurrect a canceled request.
    fn request(
        &self,
        op: ExtensionDataRequestOp,
    ) -> Result<ExtensionDataResultPayload, &'static str> {
        let (reply, receiver) = mpsc::channel();
        crate::send_worker_message(
            &self.tx,
            &self.waker,
            crate::WorkerMessage::ChatGptPlanSecret(Rpc { op, reply }),
        )
        .map_err(|_| "credential owner unavailable")?;
        receiver
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| "credential storage response timed out")
    }

    /// Read complete authoritative bytes, never hydrate from a stale cache.
    fn read(&self) -> Result<Vec<u8>, &'static str> {
        match self.request(ExtensionDataRequestOp::ReadFile {
            path: self.path.clone(),
        })? {
            ExtensionDataResultPayload::Ok {
                value: ExtensionDataValue::ReadFile { contents },
            } => Ok(contents),
            _ => Err("ChatGPT credential is unavailable; sign in again"),
        }
    }

    /// CAS cannot overwrite a winning logout/login even if it does not
    /// cooperate with this provider's lock (for example profile removal).
    fn publish(&self, previous: &[u8], next: &Credential) -> Result<(), &'static str> {
        match self.request(ExtensionDataRequestOp::CompareAndSwapFile {
            path: self.path.clone(),
            expected_generation: blake3::hash(previous).to_hex().to_string(),
            contents: next
                .encode()
                .map_err(|_| "invalid credential replacement")?,
        })? {
            ExtensionDataResultPayload::Ok {
                value: ExtensionDataValue::CompareAndSwapFile,
            } => Ok(()),
            _ => Err("credential publication was not confirmed"),
        }
    }
}

/// Acquire a token-free crash-released lock under Configure's writable instance
/// root. Both it and Secret are keyed by the same state root and instance name.
pub(crate) fn lock(
    root: &Path,
    path: &tau_proto::ExtensionDataPath,
    canceled: &mut impl FnMut() -> bool,
) -> Result<std::fs::File, &'static str> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let key = blake3::hash(path.as_str().as_bytes()).to_hex();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(format!("chatgpt-plan-{key}.lock")))
        .map_err(|_| "ChatGPT credential lock unavailable")?;
    if !file
        .metadata()
        .map_err(|_| "credential lock unavailable")?
        .is_file()
    {
        return Err("ChatGPT credential lock is not a regular file");
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if canceled() {
            return Err("ChatGPT credential lock canceled");
        }
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(_) => return Err("ChatGPT credential lock failed"),
        }
        if Instant::now() >= deadline {
            return Err("ChatGPT credentials are busy in another process; retry later");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl<F> crate::ProviderRuntime<F>
where
    F: FnMut(Option<&tau_proto::ProviderName>) -> crate::BuiltinProviderProfiles + 'static,
{
    /// Bind an admitted ChatGPT profile to exactly its instance's Secret scope.
    pub(crate) fn attach_chatgpt_plan_session(
        &self,
        backend: &mut crate::PromptBackend,
        profiles: &crate::BuiltinProviderProfiles,
        provider: &tau_proto::ProviderName,
    ) {
        let crate::PromptBackend::ChatGptPlan { profile, .. } = backend else {
            return;
        };
        let Some(root) = self.configuration.state_dir.clone() else {
            return;
        };
        let Some(waker) = self.worker_waker.clone() else {
            return;
        };
        let Some(crate::ProviderCredential::Stored(reference)) = profiles.credentials.get(provider)
        else {
            return;
        };
        Arc::make_mut(profile).session = Some(Arc::new(Session {
            root,
            path: reference.path().clone(),
            tx: self.worker_tx.clone(),
            waker,
            failed: Arc::clone(&self.credential_admission.chatgpt_plan.failed),
        }));
    }

    /// Main-loop-owned Secret RPC submission keeps client authority off
    /// workers.
    pub(crate) fn start_chatgpt_plan_secret(&mut self, rpc: Rpc) {
        let Some(client) = &self.extension_data_client else {
            return;
        };
        if let Ok(id) = client.start_request(ExtensionDataScope::Secret, rpc.op) {
            self.credential_admission
                .chatgpt_plan
                .replies
                .insert(id, rpc.reply);
        }
    }
}
