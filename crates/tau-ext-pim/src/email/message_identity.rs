//! Private exact message identities retained across email engine stages.

use std::num::NonZeroU32;

use super::{EmailAccountId, validate_mailbox_name};

/// Exact validated provider mailbox name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MailboxName(
    /// Exact accepted provider mailbox text.
    String,
);

impl MailboxName {
    /// Validate and retain an exact mailbox name without normalization.
    pub(super) fn parse_exact(raw: &str) -> Result<Self, String> {
        validate_mailbox_name(raw)?;
        Ok(Self(raw.to_owned()))
    }

    /// Return the exact validated mailbox spelling.
    pub(super) fn raw(&self) -> &str {
        &self.0
    }
}

/// Exact validated IMAP UID plus its positive numeric value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ImapUid {
    /// Exact accepted decimal spelling.
    raw: String,
    /// Parsed positive UID value.
    value: NonZeroU32,
}

impl ImapUid {
    /// Validate and retain an exact decimal IMAP UID spelling.
    pub(super) fn parse_exact(raw: &str) -> Result<Self, String> {
        let value = Self::parse_value(raw)?;
        Ok(Self {
            raw: raw.to_owned(),
            value,
        })
    }

    /// Validate a borrowed decimal IMAP UID at an existing provider parse cut.
    pub(super) fn parse_value(raw: &str) -> Result<NonZeroU32, String> {
        if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("uid must be a positive integer".to_owned());
        }
        raw.parse::<NonZeroU32>()
            .map_err(|_| "uid must be a positive integer".to_owned())
    }

    /// Return the exact accepted decimal spelling.
    pub(super) fn raw(&self) -> &str {
        &self.raw
    }
}

/// Exact opaque UIDVALIDITY text supplied by the provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct UidValidity(
    /// Exact opaque text supplied by provider metadata.
    String,
);

impl UidValidity {
    /// Retain provider UIDVALIDITY text without validation or normalization.
    pub(super) fn from_provider(raw: String) -> Self {
        Self(raw)
    }

    /// Return the exact provider UIDVALIDITY text.
    pub(super) fn raw(&self) -> &str {
        &self.0
    }
}

/// Validated configured message target before provider metadata is available.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MessageTarget {
    /// Resolved and enabled configured account identity.
    account: EmailAccountId,
    /// Exact validated mailbox name.
    mailbox: MailboxName,
    /// Exact validated message UID.
    uid: ImapUid,
}

impl MessageTarget {
    /// Build a target after account, mailbox, folder-policy, and UID
    /// validation.
    pub(super) fn new(account: EmailAccountId, mailbox: MailboxName, uid: ImapUid) -> Self {
        Self {
            account,
            mailbox,
            uid,
        }
    }

    /// Return the resolved configured account identity.
    pub(super) fn account(&self) -> &EmailAccountId {
        &self.account
    }

    /// Return the exact validated mailbox name.
    pub(super) fn mailbox(&self) -> &MailboxName {
        &self.mailbox
    }

    /// Return the exact validated message UID.
    pub(super) fn uid(&self) -> &ImapUid {
        &self.uid
    }
}

/// Validated message target paired with exact provider UIDVALIDITY metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MessageRef {
    /// Already-validated configured message target.
    target: MessageTarget,
    /// Exact opaque UIDVALIDITY supplied by metadata.
    uidvalidity: UidValidity,
}

impl MessageRef {
    /// Pair a validated target with exact provider UIDVALIDITY metadata.
    pub(super) fn from_metadata(target: MessageTarget, uidvalidity: String) -> Self {
        Self {
            target,
            uidvalidity: UidValidity::from_provider(uidvalidity),
        }
    }

    /// Return the validated target.
    pub(super) fn target(&self) -> &MessageTarget {
        &self.target
    }

    /// Return the exact provider UIDVALIDITY text.
    pub(super) fn uidvalidity(&self) -> &UidValidity {
        &self.uidvalidity
    }

    /// Return whether a body fetch retains exact metadata identity.
    pub(super) fn metadata_matches_body(
        &self,
        metadata_uid: &str,
        body_uid: &str,
        body_uidvalidity: &str,
    ) -> bool {
        metadata_uid == body_uid && self.uidvalidity.raw() == body_uidvalidity
    }
}

#[cfg(test)]
mod tests;
