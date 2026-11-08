//! Bounded admission observations, never ownership authority.

use std::fmt;
use std::path::Path;

use tau_proto::{AgentId, ToolResponse, ToolResponseHeader};

/// Display-path budget leaves ample room for worst-case header escaping.
const MAX_DIRECTORY_BYTES: usize = 512;
/// Abbreviation marker used by the shared bounded directory formatter.
const DIRECTORY_ELLIPSIS: &str = "...";

/// The admission rule responsible for one observed conflict.
pub(super) enum BlockerKind {
    /// An active owner-held manual reservation.
    Manual,
    /// An active mutation's automatic guard.
    Automatic,
    /// Path-local FIFO precedence, not current ownership.
    EarlierQueued,
}

impl BlockerKind {
    fn header(&self) -> &'static str {
        match self {
            Self::Manual => "manual_lock",
            Self::Automatic => "automatic_lock",
            Self::EarlierQueued => "earlier_queued_request",
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Manual => "manual lock",
            Self::Automatic => "automatic lock",
            Self::EarlierQueued => "earlier queued request",
        }
    }
}

/// One representative blocker observed while admitting a queued request.
pub(super) struct WaitBlocker {
    /// Contactable agent, without changing backend instance-aware identity.
    owner: AgentId,
    /// Whether admission waits for a holder or an earlier queued request.
    kind: BlockerKind,
    /// Actual overlapping canonical directory, bounded before leaving
    /// admission.
    directory: String,
    /// Display-path bytes omitted before header escaping, excluding the
    /// ellipsis.
    omitted_bytes: usize,
}

impl WaitBlocker {
    /// Capture only bounded diagnostic data while the admission guard is held.
    pub(super) fn new(owner: &AgentId, kind: BlockerKind, directory: &Path) -> Self {
        let text = directory.to_string_lossy();
        let directory =
            super::bounded_dirs_display(&[directory.to_path_buf()], MAX_DIRECTORY_BYTES);
        let content_budget = MAX_DIRECTORY_BYTES - DIRECTORY_ELLIPSIS.len();
        let omitted_bytes = if text.len() > content_budget {
            text.len()
                .saturating_sub(directory.len().saturating_sub(DIRECTORY_ELLIPSIS.len()))
        } else {
            0
        };
        Self {
            owner: owner.clone(),
            kind,
            directory,
            omitted_bytes,
        }
    }

    /// Render ordinary tool headers through the shared escaping boundary.
    pub(super) fn render(&self) -> String {
        let mut fields = vec![
            ("status", "waiting".to_owned()),
            ("blocker_snapshot", "true".to_owned()),
            ("blocker_kind", self.kind.header().to_owned()),
            ("blocker_agent", self.owner.to_string()),
            ("blocker_directory", self.directory.clone()),
        ];
        if self.omitted_bytes != 0 {
            fields.push((
                "blocker_directory_omitted_bytes",
                self.omitted_bytes.to_string(),
            ));
        }
        ToolResponse {
            raw: tau_proto::CborValue::Null,
            headers: fields
                .into_iter()
                .map(|(key, value)| ToolResponseHeader {
                    key: key.to_owned(),
                    value,
                })
                .collect(),
            body: String::new(),
        }
        .render()
    }
}

impl fmt::Display for WaitBlocker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "agent {}, {}, directory {}",
            self.owner,
            self.kind.label(),
            self.directory
        )
    }
}
