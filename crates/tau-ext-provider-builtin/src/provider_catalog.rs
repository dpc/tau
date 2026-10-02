//! Built-in provider-kind discovery and credential-qualified model
//! declarations.

use crate::{
    BuiltinComponentIdentity, BuiltinProviderProfile, BuiltinProviderProfiles, ProviderModelInfo,
    ProviderName, chat_models_for_provider, responses_models_for_provider,
};

/// Discover only enabled configured instances owned by this builtin provider.
pub(super) fn provider_cli_entry_is_builtin(
    name: &str,
    entry: &tau_config::settings::ExtensionEntry,
) -> bool {
    if entry.enable == Some(false) || entry.role.as_deref().is_some_and(|role| role != "provider") {
        return false;
    }
    let component = entry
        .command
        .is_none()
        .then(|| {
            entry
                .suffix
                .as_deref()
                .and_then(BuiltinComponentIdentity::from_tau_owned_suffix)
        })
        .flatten();
    if name == "provider-builtin" && entry.suffix.is_none() {
        return entry.command.is_none();
    }
    component == Some(BuiltinComponentIdentity::Provider)
        && (name == "provider-builtin" || entry.role.as_deref() == Some("provider"))
}

/// Public command catalog uses the same canonical kind spellings as the picker.
pub(super) const PROVIDER_CLI_HELP: &str = "\
Usage: tau provider [--extension INSTANCE] <subcommand>

Subcommands:
  add [--state|--config [--output -]] [KIND]
                                 Add or replace a provider profile (default: state)
  login <name>                   Authenticate an existing provider profile
  logout <name>                  Sign out of native Grok or ChatGPT plan registration
  remove [--state|--config] <name>
                                  Remove a provider profile
  rename <old> <new>             Rename a provider profile without changing credentials
  list [--state|--config|--all]  List provider profiles with their source
  show <name>                    Show a credential-free profile and source path

Provider kinds:
  chatgpt           ChatGPT / Codex
  chatgpt-plan      Sign in with ChatGPT / public Responses
  grok              Native Grok device OAuth / public Responses
  chat-completions  OpenAI-compatible Chat Completions
  responses         OpenAI Responses API
  openrouter        OpenRouter

chatgpt-plan requires persistent --state or --config setup; --output - is unsupported.";

/// One canonical provider-kind choice accepted by the setup command.
///
/// The token is the only non-interactive spelling. The label is deliberately
/// human-oriented because the same table drives the interactive picker.
pub(super) struct ProviderKindDescriptor {
    /// Canonical machine token accepted after `tau provider add`.
    pub(super) token: &'static str,
    /// Human-readable picker label.
    pub(super) label: &'static str,
}

/// The complete, canonical provider-kind catalog.
pub(super) const PROVIDER_KINDS: [ProviderKindDescriptor; 6] = [
    ProviderKindDescriptor {
        token: "chatgpt",
        label: "ChatGPT / Codex",
    },
    ProviderKindDescriptor {
        token: "chatgpt-plan",
        label: "Sign in with ChatGPT (public Responses, separate from Codex)",
    },
    ProviderKindDescriptor {
        token: "grok",
        label: "Grok subscription (native OAuth)",
    },
    ProviderKindDescriptor {
        token: "chat-completions",
        label: "OpenAI-compatible Chat Completions",
    },
    ProviderKindDescriptor {
        token: "responses",
        label: "OpenAI Responses API",
    },
    ProviderKindDescriptor {
        token: "openrouter",
        label: "OpenRouter",
    },
];

/// Publish each provider's admitted catalog without substituting another
/// family.
pub(super) fn models_for_profiles(profiles: &BuiltinProviderProfiles) -> Vec<ProviderModelInfo> {
    models_for_profiles_with_admission(profiles, true)
}

/// Declaration inspection describes configured capabilities without loading or
/// inferring credential usability; operational publication remains separate.
pub(super) fn models_for_inspection(profiles: &BuiltinProviderProfiles) -> Vec<ProviderModelInfo> {
    models_for_profiles_with_admission(profiles, false)
}

/// Shared metadata projection; only operational declarations require a plan
/// grant. Inspection cannot read Secret state.
fn models_for_profiles_with_admission(
    profiles: &BuiltinProviderProfiles,
    require_plan_grant: bool,
) -> Vec<ProviderModelInfo> {
    let mut models = Vec::new();
    for (provider_name, profile) in &profiles.providers {
        match profile {
            BuiltinProviderProfile::ChatgptPlan(profile) => {
                if !require_plan_grant
                    || profile
                        .credential
                        .as_ref()
                        .is_some_and(|credential| credential.access_token().is_ok())
                {
                    models.extend(responses_models_for_provider(
                        provider_name,
                        &profile.responses(),
                    ));
                }
            }
            BuiltinProviderProfile::Grok(profile) => {
                models.extend(profile.models_for_provider(provider_name));
            }
            BuiltinProviderProfile::Chatgpt(profile) => {
                models.extend(tau_provider_codex::models_for_provider_mode(
                    provider_name,
                    profile.responses_mode(),
                ));
            }
            BuiltinProviderProfile::ChatCompletions(provider) => {
                models.extend(chat_models_for_provider(provider_name, provider));
            }
            BuiltinProviderProfile::OpenRouter(profile) => {
                let provider = profile.to_chat_completions();
                models.extend(chat_models_for_provider(provider_name, &provider));
            }
            BuiltinProviderProfile::Responses(provider) => {
                models.extend(responses_models_for_provider(provider_name, provider));
            }
        }
    }
    models
}

/// Replaces one provider's models while preserving every sibling contribution
/// and deterministic provider/model ordering from a complete declaration.
pub(super) fn replace_provider_models(
    previous: &[ProviderModelInfo],
    provider: &ProviderName,
    selected_profiles: &BuiltinProviderProfiles,
) -> Vec<ProviderModelInfo> {
    let mut models = previous
        .iter()
        .filter(|model| &model.id.provider != provider)
        .cloned()
        .collect::<Vec<_>>();
    models.extend(models_for_profiles(selected_profiles));
    models.sort_by(|left, right| left.id.provider.cmp(&right.id.provider));
    models
}
