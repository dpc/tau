//! Backend identity construction and turn-wide reachability retention.

use tau_proto::{ProviderBackend, ProviderBackendKind, ProviderBackendTransport};
use tau_provider_codex::ResolvedConfig;

use crate::{ChatCompletionsProvider, ResponsesProvider};

/// Build terminal routing metadata for a reached Chat Completions backend.
pub(super) fn chat_completions_backend(provider: &ChatCompletionsProvider) -> ProviderBackend {
    ProviderBackend {
        kind: ProviderBackendKind::ChatCompletions,
        base_url: provider.base_url.clone(),
        transport: ProviderBackendTransport::HttpSse,
        stale_chain_fallback: false,
    }
}

/// Build terminal routing metadata for a reached public Responses backend.
pub(super) fn responses_backend(provider: &ResponsesProvider) -> ProviderBackend {
    ProviderBackend {
        kind: ProviderBackendKind::PublicResponses,
        base_url: provider.base_url.clone(),
        transport: match provider.transport {
            tau_provider_responses::Transport::Sse => ProviderBackendTransport::HttpSse,
            tau_provider_responses::Transport::Websocket => ProviderBackendTransport::Websocket,
        },
        stale_chain_fallback: false,
    }
}

/// Build terminal routing metadata for a reached private Codex backend.
pub(super) fn codex_backend(
    config: &ResolvedConfig,
    transport: ProviderBackendTransport,
    stale_chain_fallback: bool,
) -> ProviderBackend {
    ProviderBackend {
        kind: ProviderBackendKind::Responses,
        base_url: config.base_url().to_owned(),
        transport,
        stale_chain_fallback,
    }
}

/// Prefer the current attempt's reached backend while retaining earlier
/// turn-wide reachability when the current attempt exits before dispatch.
pub(super) fn observed_backend(
    current: Option<ProviderBackend>,
    prior: Option<&ProviderBackend>,
) -> Option<ProviderBackend> {
    current.or_else(|| prior.cloned())
}
