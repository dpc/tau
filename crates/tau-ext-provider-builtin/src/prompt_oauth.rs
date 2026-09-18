use tau_proto::ProviderName;

use crate::OpenAiAuth;

/// Provider, startup mode, and exact Secret generation used to coalesce OAuth
/// refresh.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct PromptOAuthRefreshKey {
    /// Provider namespace whose rejection and identity state owns this refresh.
    pub(super) provider: ProviderName,
    /// Opaque Secret-scope path; never rendered.
    pub(super) path: tau_proto::ExtensionDataPath,
    /// BLAKE3 generation expected by Secret compare-and-swap.
    pub(super) generation: String,
    /// Startup-selected Responses Lite mode, retained without making
    /// `CodexMode` part of the hash key.
    pub(super) lite_compatibility: bool,
}

/// Process-local identity of one installed OAuth refresh flight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PromptOAuthRefreshOwner(pub(super) u64);

/// One shared network refresh and its main-loop-owned Secret publication state.
pub(super) struct PromptOAuthRefresh {
    /// Identity required by every asynchronous transition for this flight.
    pub(super) owner: PromptOAuthRefreshOwner,
    /// Credential generation supplied to the OAuth endpoint.
    pub(super) current: OpenAiAuth,
    /// Whether this flight consumed forced recovery authority for the
    /// generation.
    pub(super) forced: bool,
    /// Whether network OAuth ended and Secret publication has begun.
    pub(super) transport_finished: bool,
    /// Whether a shared Secret CAS or authoritative reload is in flight.
    pub(super) secret_in_flight: bool,
}

/// Main-loop continuation for one prompt-owned Secret operation.
pub(super) enum PromptOAuthRpc {
    /// Publish the refreshed credential, then verify the authoritative record.
    CompareAndSwap {
        /// Exact refresh operation.
        key: PromptOAuthRefreshKey,
        /// Process-local owner of the keyed flight.
        owner: PromptOAuthRefreshOwner,
    },
    /// Adopt the authoritative record after CAS success or a losing CAS.
    Reload {
        /// Exact refresh operation.
        key: PromptOAuthRefreshKey,
        /// Process-local owner of the keyed flight.
        owner: PromptOAuthRefreshOwner,
    },
}
