//! Correlated, content-free proof that the disabled clipboard producer drained.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

#[cfg(test)]
mod tests;

/// One fresh metadata transaction, never a clipboard content acquisition.
pub(super) struct ClipboardFence {
    /// Attachment-local unique identifier, distinct from content requests.
    id: String,
    /// Whether the opening OK has arrived.
    opened: bool,
    /// Whether at least one MIME inventory DATA frame has arrived.
    data_seen: bool,
    /// Bounded inventory retained to validate UTF-8 at DONE.
    inventory: Vec<u8>,
}

impl ClipboardFence {
    /// Creates a transaction whose complete success is the only positive proof.
    pub(super) fn new(id: String) -> Self {
        Self {
            id,
            opened: false,
            data_seen: false,
            inventory: Vec::new(),
        }
    }

    /// Orders mode-off before a standard, prompt-free dot metadata read.
    pub(super) fn request(&self) -> Vec<u8> {
        format!("\x1b[?5522l\x1b]5522;type=read:id={};Lg==\x1b\\", self.id).into_bytes()
    }

    /// Ignores old transactions; rejects malformed matching replies and errors.
    pub(super) fn receive(&mut self, body: &[u8]) -> Result<bool, &'static str> {
        let Ok(body) = std::str::from_utf8(body) else {
            return Ok(false);
        };
        let Some(body) = body.strip_prefix("5522;") else {
            return Ok(false);
        };
        let (metadata, payload) = body.split_once(';').unwrap_or((body, ""));
        if !metadata
            .split(':')
            .any(|field| field.strip_prefix("id=") == Some(self.id.as_str()))
        {
            return Ok(false);
        }
        let mut fields = HashMap::new();
        for field in metadata.split(':') {
            let (key, value) = field.split_once('=').ok_or("malformed fence metadata")?;
            if fields.insert(key, value).is_some() {
                return Err("duplicate fence metadata");
            }
        }
        if fields.get("type") != Some(&"read")
            || fields.get("id").copied() != Some(self.id.as_str())
        {
            return Err("invalid fence response");
        }
        match fields.get("status").copied() {
            Some("OK") if !self.opened && payload.is_empty() => self.opened = true,
            Some("DATA") if self.opened => {
                if !body.contains(';') {
                    return Err("missing fence DATA payload separator");
                }
                if fields.get("mime") != Some(&"Lg==") || payload.len() > 5464 {
                    return Err("invalid fence MIME or chunk size");
                }
                let bytes = STANDARD
                    .decode(payload)
                    .map_err(|_| "invalid fence encoding")?;
                if bytes.len() > 4096 || self.inventory.len() + bytes.len() > 64 * 1024 {
                    return Err("fence inventory exceeds limit");
                }
                self.inventory.extend(bytes);
                self.data_seen = true;
            }
            Some("DONE") if self.opened && self.data_seen && payload.is_empty() => {
                std::str::from_utf8(&self.inventory).map_err(|_| "invalid fence inventory")?;
                return Ok(true);
            }
            _ => return Err("fence response failed or arrived out of order"),
        }
        Ok(false)
    }
}
