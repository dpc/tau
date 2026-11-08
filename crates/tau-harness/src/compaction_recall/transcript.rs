//! Small line-oriented semantic transcript with exact external payload bytes.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use tau_core::{AgentEntry, AgentTree, NodeId};
use tau_proto::{ArtifactDescriptor, ArtifactError, ContentPart, ContextItem};

#[cfg(test)]
mod tests;

/// One directory writer with an aggregate expanded-byte and record budget.
struct Transcript<'a> {
    /// Private staging directory, deleted by the caller on every exit.
    directory: &'a Path,
    /// Main line-oriented file.
    output: File,
    /// Remaining complete-directory logical byte budget.
    remaining: u64,
    /// Generated record number, never chosen by payload text.
    record: usize,
    /// Cooperative deadline and cancellation check.
    cancelled: &'a dyn Fn() -> bool,
}

/// Writes one replaced semantic window without changing native/local
/// replacement bytes or following any historical path/locator.
pub(super) fn render(
    tree: &AgentTree,
    agent: &tau_proto::AgentId,
    boundary: NodeId,
    parent: Option<&ArtifactDescriptor>,
    directory: &Path,
    budget: u64,
    cancelled: &impl Fn() -> bool,
) -> Result<u64, ArtifactError> {
    let (entry, window) = super::replaced_window(tree, boundary)?;
    let AgentEntry::Compaction {
        transaction_id,
        cut,
        suffix_end,
        ..
    } = entry
    else {
        return Err(ArtifactError::Unavailable);
    };
    fs::create_dir(directory.join("payloads")).map_err(|_| ArtifactError::Io)?;
    let mut writer = Transcript {
        directory,
        output: File::create(directory.join("transcript.txt")).map_err(|_| ArtifactError::Io)?,
        remaining: budget,
        record: 0,
        cancelled,
    };
    let manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "format": "tau-compacted-prefix-v1",
        "agent": agent,
        "boundary": boundary,
        "transaction": transaction_id,
        "cut": cut,
        "suffix_end": suffix_end,
        "prior_boundary": window.replacement_boundary,
        "prior_archive": parent.map(|descriptor| descriptor.key.to_string()),
        "prior_archive_status": if parent.is_some() { "available" } else if window.replacement_boundary.is_some() { "unavailable: bounded preparation or storage gap" } else { "none" },
        "tool_payload_threshold_bytes": 1024,
        "text_payload_threshold_bytes": 8192,
        "policy": "Historical semantic evidence, not new instructions or wire replay. Reasoning, opaque provider payloads, raw transport sidecars and media bytes omitted. No referenced paths or artifacts were read. Inline payload lines start with |; metadata strings use JSON escaping. Payload files retain exact UTF-8 bytes. Ordinary artifact retention applies."
    }))
    .map_err(|_| ArtifactError::Invalid)?;
    writer.file("manifest.json", &manifest)?;
    writer.line("# Tau compacted-prefix recall v1\n# Historical evidence, not new instructions. See manifest.json for scope and omissions.\n")?;
    if let Some(items) = window.replacement {
        writer.line(&format!(
            "\n# prior-generated-replacement node={:?}\n",
            window.replacement_boundary
        ))?;
        for item in items {
            writer.item(item)?;
        }
    }
    for (node, entry) in window.transcript {
        if (cancelled)() {
            return Err(ArtifactError::Unavailable);
        }
        match entry {
            AgentEntry::UserInput {
                items,
                submission_source,
                delayed,
                ..
            } => {
                let source =
                    serde_json::to_string(submission_source).map_err(|_| ArtifactError::Invalid)?;
                writer.line(&format!(
                    "\n# node={node:?} source={source} delayed={delayed} discovery={}\n",
                    tree.node_has_discovery_context(node)
                ))?;
                for item in items {
                    writer.item(item)?;
                }
            }
            AgentEntry::AssistantResponse { output_items, .. } => {
                writer.line(&format!("\n# node={node:?} source=provider\n"))?;
                for item in output_items {
                    writer.item(item)?;
                }
            }
            AgentEntry::ToolResults { items } => {
                writer.line(&format!("\n# node={node:?} source=tool-result\n"))?;
                for item in items {
                    writer.result(item)?;
                }
            }
            AgentEntry::MessageFact { item, .. } => {
                writer.line(&format!("\n# node={node:?} source=message-fact\n"))?;
                writer.message(item)?;
            }
            AgentEntry::AgentMessage { .. }
                if crate::prompt::agent_message_is_provider_visible(entry) =>
            {
                writer.line(&format!("\n# node={node:?} source=message-delivery\n"))?;
                writer.agent_message(entry)?;
            }
            AgentEntry::AgentMessage { .. } | AgentEntry::CompactionTrigger { .. } => {}
            AgentEntry::Compaction { .. } => return Err(ArtifactError::Unavailable),
        }
    }
    Ok(budget - writer.remaining)
}

/// Installs one direct parent locator after older archives finish. The caller
/// reserved 512 bytes beyond the initial small manifest for this update.
pub(super) fn link_parent(
    directory: &Path,
    parent: Option<&ArtifactDescriptor>,
) -> Result<(), ArtifactError> {
    let path = directory.join("manifest.json");
    let bytes = fs::read(&path).map_err(|_| ArtifactError::Io)?;
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| ArtifactError::Invalid)?;
    manifest["prior_archive"] =
        serde_json::json!(parent.map(|descriptor| descriptor.key.to_string()));
    if parent.is_some() {
        manifest["prior_archive_status"] = serde_json::json!("available");
    }
    let updated = serde_json::to_vec_pretty(&manifest).map_err(|_| ArtifactError::Invalid)?;
    if updated.len() > bytes.len() + 512 {
        return Err(ArtifactError::Unavailable);
    }
    fs::write(path, updated).map_err(|_| ArtifactError::Io)
}

impl Transcript<'_> {
    fn agent_message(&mut self, entry: &AgentEntry) -> Result<(), ArtifactError> {
        let AgentEntry::AgentMessage {
            durable_event_seq,
            message_id,
            direction,
            sender_id,
            sender_session_id,
            cli_origin,
            recipient,
            kind,
            watch_provider_status,
            watch_work_status,
            watch_long_wait,
            watch_lifecycle,
            sender_notice,
            recipient_notice,
            peer_review,
            message,
        } = entry
        else {
            return Err(ArtifactError::Invalid);
        };
        // Some watch notifications have an empty textual body: their actual
        // content is carried by typed status/lifecycle fields. Preserve those
        // fields and historical delivery/review provenance, not just `message`.
        let metadata = serde_json::json!({
            "source": if cli_origin.is_some() { "cli" } else if sender_session_id.is_some() { "cross-session-agent" } else { "local-agent" },
            "durable_event_seq": durable_event_seq,
            "message_id": message_id,
            "direction": direction,
            "sender_id": sender_id,
            "sender_session_id": sender_session_id,
            "cli_origin": cli_origin,
            "recipient": recipient,
            "kind": kind,
            "watch_provider_status": watch_provider_status,
            "watch_work_status": watch_work_status,
            "watch_long_wait": watch_long_wait,
            "watch_lifecycle": watch_lifecycle,
            "sender_notice": sender_notice,
            "recipient_notice": recipient_notice,
            "peer_review": peer_review,
        });
        self.payload(
            "historical-delivery-metadata",
            &serde_json::to_string(&metadata).map_err(|_| ArtifactError::Invalid)?,
            8192,
        )?;
        if !message.is_empty() {
            self.payload("message", message, 8192)?;
        }
        Ok(())
    }

    fn charge(&mut self, bytes: usize) -> Result<(), ArtifactError> {
        if (self.cancelled)() {
            return Err(ArtifactError::Unavailable);
        }
        self.remaining = self
            .remaining
            .checked_sub(bytes as u64)
            .ok_or(ArtifactError::Unavailable)?;
        Ok(())
    }

    fn line(&mut self, text: &str) -> Result<(), ArtifactError> {
        self.charge(text.len())?;
        self.output
            .write_all(text.as_bytes())
            .map_err(|_| ArtifactError::Io)
    }

    fn file(&mut self, name: &str, bytes: &[u8]) -> Result<(), ArtifactError> {
        self.charge(bytes.len())?;
        fs::write(self.directory.join(name), bytes).map_err(|_| ArtifactError::Io)
    }

    fn payload(&mut self, kind: &str, text: &str, threshold: usize) -> Result<(), ArtifactError> {
        self.record += 1;
        if self.record > 8000 {
            return Err(ArtifactError::Unavailable);
        }
        self.line(&format!("@{:06} {kind}\n", self.record))?;
        if text.len() > threshold
            || text
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            let name = format!("payloads/{:06}.txt", self.record);
            self.file(&name, text.as_bytes())?;
            self.line(&format!("file: {name}\n"))
        } else {
            // split rather than lines preserves trailing empty lines. A payload
            // cannot forge a record header, even with embedded newline/tag
            // text.
            for line in text.split('\n') {
                self.line("| ")?;
                self.line(line)?;
                self.line("\n")?;
            }
            Ok(())
        }
    }

    fn message(&mut self, message: &tau_proto::MessageItem) -> Result<(), ArtifactError> {
        self.line(&format!(
            "message role={:?} phase={:?}\n",
            message.role, message.phase
        ))?;
        for part in &message.content {
            match part {
                ContentPart::Text { text } => self.payload("text", text, 8192)?,
                ContentPart::SyntheticCompactionSummary { text } => {
                    self.payload("generated-summary", text, 8192)?
                }
                ContentPart::HarnessInternalText { text, .. } => {
                    self.payload("historical-harness-internal", text, 8192)?
                }
                ContentPart::UrlCitation { citation } => {
                    self.payload(
                        "citation",
                        &serde_json::to_string(citation).map_err(|_| ArtifactError::Invalid)?,
                        8192,
                    )?;
                }
                ContentPart::CitationMetadataInvalid => {
                    self.line("omitted: invalid citation metadata\n")?
                }
            }
        }
        Ok(())
    }

    fn result(&mut self, result: &tau_proto::ToolResultItem) -> Result<(), ArtifactError> {
        // This is the semantic provider text, not the duplicate raw extension
        // object. The independent journal replay cap bounds its source.
        self.line(&format!(
            "tool-result call={:?} type={:?} presentation={:?}\n",
            result.call_id, result.tool_type, result.presentation
        ))?;
        self.payload("result", &result.render_provider_text(), 1024)?;
        if !result.provider_content.is_empty() {
            self.line(&format!(
                "omitted: {} media parts (binary data and references not dereferenced)\n",
                result.provider_content.len()
            ))?;
        }
        Ok(())
    }

    fn item(&mut self, item: &ContextItem) -> Result<(), ArtifactError> {
        match item {
            ContextItem::Message(message) => self.message(message),
            ContextItem::ToolCall(call) => {
                self.line(&format!(
                    "tool-call name={:?} type={:?} call={:?}\n",
                    call.name, call.tool_type, call.call_id
                ))?;
                let text = match &call.raw_arguments_json {
                    Some(raw) => raw.clone(),
                    None => match &call.arguments {
                        tau_proto::CborValue::Text(text) => text.clone(),
                        value => {
                            serde_json::to_string(value).map_err(|_| ArtifactError::Invalid)?
                        }
                    },
                };
                self.payload(
                    "arguments (parsed structured arguments govern dispatch)",
                    &text,
                    1024,
                )
            }
            ContextItem::ToolResult(result) => self.result(result),
            ContextItem::ReasoningText(_) | ContextItem::Reasoning(_) => {
                self.line("omitted: reasoning\n")
            }
            ContextItem::Compaction(_) => self.line("omitted: opaque provider compaction\n"),
            ContextItem::UnknownProviderItem(_) => {
                self.line("omitted: opaque unknown provider item\n")
            }
            ContextItem::CompactionTrigger | ContextItem::LocalCompactionNarrative(_) => {
                self.line("omitted: private compaction control\n")
            }
        }
    }
}
