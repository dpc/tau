//! Closed public labels for recognized Codex request rejections.

/// An explicit provider rejection whose fixed label is safe to show to users.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestRejectionReason {
    /// The provider rejected the request under its cyber policy.
    CyberPolicy,
    /// The provider rejected the request under its biological safety policy.
    BioPolicy,
    /// The provider rejected the prompt as invalid.
    InvalidPrompt,
    /// The requested model does not exist or is unavailable to this account.
    ModelNotFound,
}

impl RequestRejectionReason {
    /// Recognize only the exact codes already treated as terminal rejections.
    pub(crate) fn from_code(code: Option<&str>) -> Option<Self> {
        match code {
            Some("cyber_policy") => Some(Self::CyberPolicy),
            Some("bio_policy") => Some(Self::BioPolicy),
            Some("invalid_prompt") => Some(Self::InvalidPrompt),
            Some("model_not_found") => Some(Self::ModelNotFound),
            _ => None,
        }
    }

    /// Return a fixed local label without exposing provider-authored text.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CyberPolicy => "cyber_policy",
            Self::BioPolicy => "bio_policy",
            Self::InvalidPrompt => "invalid_prompt",
            Self::ModelNotFound => "model_not_found",
        }
    }
}
