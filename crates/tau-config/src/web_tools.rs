//! Logical web capability configuration and merge policy.

use std::collections::{BTreeMap, BTreeSet};

use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use tau_proto::ToolName;

use super::settings::present_option;

/// Harness policy for selecting one implementation of each logical web tool.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RawWebToolsPolicy {
    /// Central allowed-domain patch: absent inherits, null clears, and a list
    /// replaces.
    #[serde(default, deserialize_with = "present_option")]
    pub(crate) allowed_domains: Option<Option<Vec<String>>>,
    /// Logical web-search policy.
    pub(crate) search: RawLogicalWebToolPolicy,
    /// Logical caller-directed web-fetch policy.
    pub(crate) fetch: RawLogicalWebToolPolicy,
}

impl RawWebToolsPolicy {
    /// Merge a higher-precedence role patch by candidate name.
    pub(super) fn apply_patch(&mut self, patch: &Self) {
        if patch.allowed_domains.is_some() {
            self.allowed_domains = patch.allowed_domains.clone();
        }
        self.search.apply_patch(&patch.search);
        self.fetch.apply_patch(&patch.fetch);
    }

    /// Validate one effective role policy after all keyed candidate merges.
    pub(super) fn validate(&self, path: &str) -> Result<(), String> {
        if let Some(Some(domains)) = &self.allowed_domains {
            if domains.len() > 100 {
                return Err(format!(
                    "{path}.allowed_domains: at most 100 domains are allowed"
                ));
            }
            let mut seen = BTreeSet::new();
            for (index, domain) in domains.iter().enumerate() {
                let field = format!("{path}.allowed_domains[{index}]");
                if domain.is_empty()
                    || domain.len() > 253
                    || domain != &domain.to_ascii_lowercase()
                    || domain.starts_with('.')
                    || domain.ends_with('.')
                    || domain.contains("://")
                    || domain.contains(['/', '?', '#', ':', '@', '*'])
                    || domain.parse::<std::net::IpAddr>().is_ok()
                    || domain.split('.').any(|label| {
                        label.is_empty()
                            || label.len() > 63
                            || label.starts_with('-')
                            || label.ends_with('-')
                            || !label.bytes().all(|byte| {
                                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                            })
                    })
                {
                    return Err(format!("{field}: expected a lowercase DNS domain name"));
                }
                if !seen.insert(domain) {
                    return Err(format!("{field}: duplicate domain `{domain}`"));
                }
            }
        }
        self.search.validate(&format!("{path}.search"), true)?;
        self.fetch.validate(&format!("{path}.fetch"), false)
    }

    /// Resolve a fully merged raw policy into its validated runtime form.
    pub(super) fn resolve(&self, path: &str) -> Result<WebToolsPolicy, String> {
        self.validate(path)?;
        Ok(WebToolsPolicy {
            allowed_domains: self.allowed_domains.clone().flatten(),
            search: self.search.resolve(),
            fetch: self.fetch.resolve(),
            raw: self.clone(),
        })
    }
}

/// Validated effective policy for selecting one implementation of each logical
/// web capability.
#[derive(Clone, Default, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebToolsPolicy {
    /// Central exact/subdomain restriction, or no restriction.
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_domains: Option<Vec<String>>,
    /// Effective logical search candidates.
    search: LogicalWebToolPolicy,
    /// Effective logical fetch candidates.
    fetch: LogicalWebToolPolicy,
    /// Private raw merge authority retained only while settings layers replay.
    #[serde(skip)]
    raw: RawWebToolsPolicy,
}

impl std::fmt::Debug for WebToolsPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebToolsPolicy")
            .field("allowed_domains", &self.allowed_domains)
            .field("search", &self.search)
            .field("fetch", &self.fetch)
            .finish()
    }
}

impl PartialEq for WebToolsPolicy {
    fn eq(&self, other: &Self) -> bool {
        self.allowed_domains == other.allowed_domains
            && self.search == other.search
            && self.fetch == other.fetch
    }
}

impl Eq for WebToolsPolicy {}

impl<'de> Deserialize<'de> for WebToolsPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawWebToolsPolicy::deserialize(deserializer)?;
        if raw == RawWebToolsPolicy::default() {
            return Ok(Self::default());
        }
        raw.resolve("web_tools").map_err(D::Error::custom)
    }
}

impl WebToolsPolicy {
    /// Merge one higher-precedence raw settings layer.
    pub(super) fn apply_patch(&mut self, patch: &RawWebToolsPolicy) {
        self.raw.apply_patch(patch);
        if let Ok(resolved) = self.raw.resolve("web_tools") {
            *self = resolved;
        }
    }

    /// Validate and replace the effective projection after all layers merge.
    pub(super) fn finalize(&mut self, path: &str) -> Result<(), String> {
        *self = self.raw.resolve(path)?;
        Ok(())
    }

    /// Effective allowed-domain restriction after role merging.
    #[must_use]
    pub fn allowed_domains(&self) -> Option<&[String]> {
        self.allowed_domains.as_deref()
    }

    /// Effective logical search policy.
    #[must_use]
    pub const fn search(&self) -> &LogicalWebToolPolicy {
        &self.search
    }

    /// Effective logical fetch policy.
    #[must_use]
    pub const fn fetch(&self) -> &LogicalWebToolPolicy {
        &self.fetch
    }

    /// Ordinary internal tool names declared by either logical capability.
    pub fn declared_tool_names(&self) -> impl Iterator<Item = &ToolName> {
        self.search
            .candidates
            .values()
            .chain(self.fetch.candidates.values())
            .filter_map(|candidate| match candidate {
                WebToolCandidate::Tool { tool, .. } => Some(tool),
                WebToolCandidate::ModelProvider { .. } => None,
            })
    }
}

/// Policy used when no implementation of a logical web capability is eligible.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebToolUnavailablePolicy {
    /// Omit the logical capability from the provider-visible prompt.
    #[default]
    Omit,
    /// Reject prompt materialization before provider delivery.
    Error,
}

/// Candidate set for one logical web capability.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RawLogicalWebToolPolicy {
    /// Optional higher-precedence unavailable-policy replacement.
    pub(crate) unavailable: Option<WebToolUnavailablePolicy>,
    /// Named candidates, merged by name across role/profile layers.
    #[serde(default, deserialize_with = "present_candidate_map")]
    pub(crate) candidates: Option<BTreeMap<String, WebToolCandidatePatch>>,
}

impl RawLogicalWebToolPolicy {
    /// Merge one higher-precedence logical-capability patch.
    pub(super) fn apply_patch(&mut self, patch: &Self) {
        if patch.unavailable.is_some() {
            self.unavailable = patch.unavailable;
        }
        for (name, candidate) in patch.candidates.iter().flatten() {
            self.candidates
                .get_or_insert_with(BTreeMap::new)
                .entry(name.clone())
                .or_default()
                .apply_patch(candidate);
        }
    }

    /// Validate effective candidates for one logical operation.
    pub(super) fn validate(&self, path: &str, allows_model_provider: bool) -> Result<(), String> {
        let Some(candidates) = &self.candidates else {
            return Err(format!(
                "{path}.candidates: candidate map must not be empty"
            ));
        };
        for (name, candidate) in candidates {
            if candidate.enable == Some(false) {
                continue;
            }
            let candidate_path = format!("{path}.candidates.{name}");
            if candidate.priority.is_none() {
                return Err(format!("{candidate_path}.priority: field is required"));
            }
            match candidate.kind {
                Some(WebToolCandidateKind::Tool) => {
                    if candidate.tool.is_none() {
                        return Err(format!("{candidate_path}.tool: field is required"));
                    }
                    if candidate.access.is_some() || candidate.context_size.is_some() {
                        return Err(format!(
                            "{candidate_path}: tool candidates cannot set access or context_size"
                        ));
                    }
                }
                Some(WebToolCandidateKind::ModelProvider) if allows_model_provider => {
                    if candidate.tool.is_some() {
                        return Err(format!(
                            "{candidate_path}.tool: model_provider candidates cannot name a tool"
                        ));
                    }
                }
                Some(WebToolCandidateKind::ModelProvider) => {
                    return Err(format!(
                        "{candidate_path}.kind: model_provider does not implement logical fetch"
                    ));
                }
                None => return Err(format!("{candidate_path}.kind: field is required")),
            }
        }
        Ok(())
    }

    fn resolve(&self) -> LogicalWebToolPolicy {
        LogicalWebToolPolicy {
            unavailable: self.unavailable.unwrap_or_default(),
            candidates: self
                .candidates
                .iter()
                .flatten()
                .filter_map(|(name, candidate)| {
                    candidate
                        .resolved()
                        .map(|resolved| (name.clone(), resolved))
                })
                .collect(),
        }
    }
}

fn present_candidate_map<'de, D>(
    deserializer: D,
) -> Result<Option<BTreeMap<String, WebToolCandidatePatch>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let candidates = BTreeMap::deserialize(deserializer)?;
    if candidates.is_empty() {
        return Err(D::Error::custom("candidate map must not be empty"));
    }
    Ok(Some(candidates))
}

/// Validated candidate set for one logical web capability.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct LogicalWebToolPolicy {
    /// Behavior when no candidate is eligible.
    #[serde(skip_serializing_if = "web_unavailable_is_omit")]
    unavailable: WebToolUnavailablePolicy,
    /// Validated candidates keyed by their deterministic merge names.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    candidates: BTreeMap<String, WebToolCandidate>,
}

const fn web_unavailable_is_omit(value: &WebToolUnavailablePolicy) -> bool {
    matches!(value, WebToolUnavailablePolicy::Omit)
}

impl LogicalWebToolPolicy {
    /// Effective behavior when no candidate is eligible.
    #[must_use]
    pub const fn unavailable(&self) -> WebToolUnavailablePolicy {
        self.unavailable
    }

    /// Return effective candidates with their deterministic merge keys.
    pub fn candidates(&self) -> impl Iterator<Item = (&str, &WebToolCandidate)> {
        self.candidates
            .iter()
            .map(|(name, candidate)| (name.as_str(), candidate))
    }
}

/// Implementation class for one logical web-tool candidate.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WebToolCandidateKind {
    /// Hosted tool supplied by the exact selected model route.
    ModelProvider,
    /// Ordinary registered Tau function tool.
    Tool,
}

/// One named logical web-tool implementation candidate.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct WebToolCandidatePatch {
    /// Whether this candidate participates in selection.
    pub enable: Option<bool>,
    /// Lower values win; names deterministically break ties.
    pub priority: Option<i64>,
    /// Candidate implementation class.
    pub kind: Option<WebToolCandidateKind>,
    /// Internal Tau tool name for `kind: tool`.
    pub tool: Option<ToolName>,
    /// Hosted-search access mode for `kind: model_provider`.
    pub access: Option<WebSearchAccess>,
    /// Hosted-search context-size patch: absent inherits and null uses provider
    /// default.
    #[serde(default, deserialize_with = "present_option")]
    pub context_size: Option<Option<tau_proto::WebSearchContextSize>>,
}

impl WebToolCandidatePatch {
    /// Merge a higher-precedence same-name candidate patch field by field.
    pub(super) fn apply_patch(&mut self, patch: &Self) {
        if patch.enable.is_some() {
            self.enable = patch.enable;
        }
        if patch.priority.is_some() {
            self.priority = patch.priority;
        }
        if patch.kind.is_some() {
            self.kind = patch.kind;
        }
        if patch.tool.is_some() {
            self.tool = patch.tool.clone();
        }
        if patch.access.is_some() {
            self.access = patch.access;
        }
        if patch.context_size.is_some() {
            self.context_size = patch.context_size;
        }
    }
}

/// One validated logical web implementation candidate.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebToolCandidate {
    /// Provider-hosted implementation on the exact selected model route.
    ModelProvider {
        /// Whether selection may use this candidate.
        enable: bool,
        /// Lower values win.
        priority: i64,
        /// Hosted source access mode.
        access: WebSearchAccess,
        /// Optional qualitative provider search context.
        context_size: Option<tau_proto::WebSearchContextSize>,
    },
    /// Ordinary registered Tau function tool.
    Tool {
        /// Whether selection may use this candidate.
        enable: bool,
        /// Lower values win.
        priority: i64,
        /// Internal registered tool name.
        tool: ToolName,
    },
}

impl WebToolCandidate {
    /// Whether the candidate participates in selection.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        match self {
            Self::ModelProvider { enable, .. } | Self::Tool { enable, .. } => *enable,
        }
    }

    /// Deterministic numeric priority.
    #[must_use]
    pub const fn priority(&self) -> i64 {
        match self {
            Self::ModelProvider { priority, .. } | Self::Tool { priority, .. } => *priority,
        }
    }
}

impl WebToolCandidatePatch {
    /// Resolve one final validated patch into its data-carrying runtime shape.
    fn resolved(&self) -> Option<WebToolCandidate> {
        match self.kind? {
            WebToolCandidateKind::ModelProvider => Some(WebToolCandidate::ModelProvider {
                enable: self.enable.unwrap_or(true),
                priority: self.priority?,
                access: self.access.unwrap_or_default(),
                context_size: self.context_size.flatten(),
            }),
            WebToolCandidateKind::Tool => Some(WebToolCandidate::Tool {
                enable: self.enable.unwrap_or(true),
                priority: self.priority?,
                tool: self.tool.clone()?,
            }),
        }
    }
}

/// Whether provider-hosted web search uses indexed data or may access live
/// pages.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchAccess {
    /// Restrict the hosted service to cached/indexed material.
    #[default]
    Cached,
    /// Permit current external web access.
    Live,
}
