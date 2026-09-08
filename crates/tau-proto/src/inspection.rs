//! Cooperative declaration-only startup, separate from runtime readiness.

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

/// Startup purpose selected only after the peer advertises inspection support.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurePurpose {
    /// Preserve ordinary configuration, initialization and readiness.
    #[default]
    Runtime,
    /// Compute declarations without initializing operational state.
    DeclarationInspection,
}

impl ConfigurePurpose {
    /// Whether this is the legacy, omitted-on-wire runtime purpose.
    #[must_use]
    pub fn is_runtime(&self) -> bool {
        matches!(self, Self::Runtime)
    }
}

/// Explicit reasons why declaration collection is incomplete.
///
/// Closed reason codes keep runtime/configuration error text and secrets out of
/// inspection diagnostics. Runtime verification is never implied by completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionGap {
    /// Some declarations require operational state or live service discovery.
    RuntimeDeclarations,
    /// Session and agent context discovery is outside this protocol.
    ContextDiscovery,
    /// The supplied configuration could not produce a declaration inventory.
    InvalidConfiguration,
}

/// One extension's terminal, config-derived inventory.
///
/// These are declarations, not published events or a model-visible effective
/// snapshot. No lifecycle events, runtime handles or raw config are included.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InspectionComplete {
    /// Logical registrations, scoped by the SDK before transmission.
    pub tools: Vec<crate::ToolRegistrationDeclared>,
    /// Pure initial prompt contributions, without session or project discovery.
    pub prompt_fragments: Vec<crate::ExtPromptFragmentPublish>,
    /// Config-derived provider routes, not live credential/service validation.
    pub providers: Vec<InspectionProviderModels>,
    /// Missing portions; empty means declaration-complete, not
    /// runtime-verified.
    pub gaps: Vec<InspectionGap>,
}

/// Config-derived provider metadata without the runtime declaration's promise
/// of accepted local configuration or usable credentials.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InspectionProviderModels {
    /// Candidate routes and capabilities; runtime acceptance remains
    /// unverified.
    pub models: Vec<crate::ProviderModelInfo>,
}
