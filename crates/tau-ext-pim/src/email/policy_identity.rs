use std::collections::BTreeSet;

use globset::GlobMatcher;
use regex::Regex;

use super::{
    AddressPattern, ValidatedIncomingAuthPolicy, ValidatedPolicy, is_unsafe_format_control,
};

/// A mailbox address normalized by the email policy's exact existing algorithm.
#[derive(Clone, Eq, PartialEq)]
pub(super) struct NormalizedEmailAddress(String);

impl NormalizedEmailAddress {
    /// Parse and normalize an address without broadening the accepted syntax.
    pub(super) fn parse(input: &str) -> Option<Self> {
        let raw = input.trim();
        let candidate = if let (Some(start), Some(end)) = (raw.rfind('<'), raw.rfind('>')) {
            if start < end {
                &raw[start + 1..end]
            } else {
                raw
            }
        } else {
            raw
        };
        let candidate = candidate.trim().trim_matches('"');
        let (local, domain) = candidate.split_once('@')?;
        if local.is_empty()
            || domain.is_empty()
            || candidate.contains(char::is_whitespace)
            || candidate
                .chars()
                .any(|ch| ch.is_control() || is_unsafe_format_control(ch))
            || candidate.matches('@').count() != 1
        {
            return None;
        }
        Some(Self(format!(
            "{}@{}",
            local.to_ascii_lowercase(),
            domain.to_ascii_lowercase()
        )))
    }

    /// Return the unchanged normalized address bytes.
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    /// Return the normalized visible domain as an exact comparison key.
    pub(super) fn domain(&self) -> DomainName {
        let (_, domain) = self
            .0
            .split_once('@')
            .expect("normalized email address retains exactly one at sign");
        DomainName::comparison_key(domain)
    }
}

/// An ASCII-folded exact Authentication-Results server comparison key.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct AuthservId(String);

impl AuthservId {
    /// Retain an already validated and folded configured authserv-id.
    fn from_validated_config(value: &str) -> Self {
        Self(value.to_owned())
    }

    /// Build an observed comparison key without trimming or validating
    /// evidence.
    pub(super) fn observed(value: &str) -> Self {
        Self(value.to_ascii_lowercase())
    }
}

/// An ASCII-folded exact domain-alignment comparison key.
#[derive(Eq, PartialEq)]
pub(super) struct DomainName(String);

impl DomainName {
    /// Build a comparison key without DNS, IDNA, trimming, or syntax
    /// validation.
    pub(super) fn comparison_key(value: &str) -> Self {
        Self(value.to_ascii_lowercase())
    }
}

/// A private typed mirror of validated incoming authentication policy.
pub(super) struct TypedIncomingAuthPolicy {
    /// Whether incoming allow decisions require trusted authentication
    /// evidence.
    require: bool,
    /// Trusted configured authserv comparison keys.
    trusted_authserv_ids: BTreeSet<AuthservId>,
    /// Whether an aligned DMARC pass alone satisfies authentication policy.
    allow_dmarc_only: bool,
}

impl TypedIncomingAuthPolicy {
    /// Project already validated public policy without new validation.
    fn from_validated(policy: &ValidatedIncomingAuthPolicy) -> Self {
        Self {
            require: policy.require,
            trusted_authserv_ids: policy
                .trusted_authserv_ids
                .iter()
                .map(|id| AuthservId::from_validated_config(id))
                .collect(),
            allow_dmarc_only: policy.allow_dmarc_only,
        }
    }

    /// Return whether authentication evidence is required.
    pub(super) fn require(&self) -> bool {
        self.require
    }

    /// Return whether the observed authserv key is trusted.
    pub(super) fn trusts(&self, observed: &AuthservId) -> bool {
        self.trusted_authserv_ids.contains(observed)
    }

    /// Return whether aligned DMARC alone satisfies the policy.
    pub(super) fn allow_dmarc_only(&self) -> bool {
        self.allow_dmarc_only
    }
}

/// A private typed mirror of one already compiled address pattern.
pub(super) enum TypedAddressPattern {
    /// Exact normalized `local@domain` match.
    Exact {
        /// Typed normalized address used for equality.
        pattern: NormalizedEmailAddress,
    },
    /// Whole-address glob match.
    Glob {
        /// Existing ASCII-folded public pattern text.
        pattern: String,
        /// Existing compiled glob matcher.
        matcher: GlobMatcher,
    },
    /// Whole-address case-sensitive regex match.
    Regex {
        /// Existing public regex source text.
        pattern: String,
        /// Existing compiled anchored regex.
        regex: Regex,
    },
}

impl TypedAddressPattern {
    /// Project an already compiled public pattern without recompilation.
    pub(super) fn from_validated(pattern: &AddressPattern) -> Self {
        match pattern {
            AddressPattern::Exact { pattern } => Self::Exact {
                pattern: NormalizedEmailAddress(pattern.clone()),
            },
            AddressPattern::Glob { pattern, matcher } => Self::Glob {
                pattern: pattern.clone(),
                matcher: matcher.clone(),
            },
            AddressPattern::Regex { pattern, regex } => Self::Regex {
                pattern: pattern.clone(),
                regex: regex.clone(),
            },
        }
    }

    /// Match one already normalized address.
    pub(super) fn matches(&self, address: &NormalizedEmailAddress) -> bool {
        match self {
            Self::Exact { pattern } => pattern == address,
            Self::Glob { matcher, .. } => matcher.is_match(address.as_str()),
            Self::Regex { regex, .. } => regex.is_match(address.as_str()),
        }
    }

    /// Return the exact existing public matched-pattern projection.
    pub(super) fn pattern_text(&self) -> &str {
        match self {
            Self::Exact { pattern } => pattern.as_str(),
            Self::Glob { pattern, .. } | Self::Regex { pattern, .. } => pattern,
        }
    }
}

/// A private typed mirror of the complete validated email policy.
pub(super) struct TypedPolicy {
    /// Configured incoming address patterns in existing evaluation order.
    incoming_allow: Vec<TypedAddressPattern>,
    /// Typed incoming authentication policy.
    incoming_auth: TypedIncomingAuthPolicy,
    /// Configured outgoing address patterns in existing evaluation order.
    outgoing_allow: Vec<TypedAddressPattern>,
}

impl TypedPolicy {
    /// Project a fully validated public policy without new fallible work.
    pub(super) fn from_validated(policy: &ValidatedPolicy) -> Self {
        Self {
            incoming_allow: policy
                .incoming_allow
                .iter()
                .map(TypedAddressPattern::from_validated)
                .collect(),
            incoming_auth: TypedIncomingAuthPolicy::from_validated(&policy.incoming_auth),
            outgoing_allow: policy
                .outgoing_allow
                .iter()
                .map(TypedAddressPattern::from_validated)
                .collect(),
        }
    }

    /// Return configured incoming patterns in their existing order.
    pub(super) fn incoming_allow(&self) -> &[TypedAddressPattern] {
        &self.incoming_allow
    }

    /// Return the typed incoming authentication policy.
    pub(super) fn incoming_auth(&self) -> &TypedIncomingAuthPolicy {
        &self.incoming_auth
    }

    /// Return configured outgoing patterns in their existing order.
    pub(super) fn outgoing_allow(&self) -> &[TypedAddressPattern] {
        &self.outgoing_allow
    }
}
