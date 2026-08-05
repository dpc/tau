/// Configured proactive Zulip direct-message destination.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProactiveDirectMessageConfig {
    /// Operator-chosen proactive destination alias.
    alias: String,
    /// Exact non-bot Zulip recipient for this direct-message destination.
    recipient: u64,
    /// Optional trusted operator description returned by discovery.
    description: Option<String>,
}

impl ProactiveDirectMessageConfig {
    /// Validate this destination and convert it to extension-private authority.
    pub(crate) fn validate(
        self,
        destination_aliases: &mut std::collections::HashSet<String>,
    ) -> Result<DirectRoute, String> {
        super::validate_alias(&self.alias)?;
        super::validate_description(self.description.as_deref())?;
        if self.recipient == 0 {
            return Err(
                "zulip proactive direct-message recipient must be a non-zero user ID".to_owned(),
            );
        }
        if !destination_aliases.insert(self.alias.clone()) {
            return Err("zulip proactive direct-message aliases must be unique".to_owned());
        }
        Ok(DirectRoute {
            alias: self.alias,
            recipient: self.recipient,
            description: self.description,
        })
    }
}

/// Validated direct-message destination retained as extension-private
/// authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectRoute {
    /// Operator alias selecting this fixed recipient.
    alias: String,
    /// Exact non-bot Zulip recipient ID.
    recipient: u64,
    /// Trusted bounded description.
    description: Option<String>,
}

impl DirectRoute {
    /// Return this destination's operator alias.
    pub(crate) fn alias(&self) -> &str {
        &self.alias
    }

    /// Return the fixed configured recipient ID.
    pub(crate) fn recipient(&self) -> u64 {
        self.recipient
    }

    /// Return the optional trusted operator description.
    pub(crate) fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
}
