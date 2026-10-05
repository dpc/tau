//! User-gesture-only OSC5522 reads. No clipboard authority or payload logging.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::clipboard_fence::ClipboardFence;

#[cfg(test)]
mod tests;

/// Maximum aggregate bytes admitted into Tau's artifact ingestion path.
const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Maximum independently decoded read chunk, as defined by OSC5522.
const MAX_CHUNK: usize = 4096;

/// Completed clipboard content; PNG never enters the editable text buffer.
#[derive(Clone)]
pub enum PasteContent {
    /// Normalized text follows the existing text-paste threshold policy.
    Text(Arc<str>),
    /// Opaque PNG bytes uploaded through the ordinary artifact protocol.
    Png(Arc<[u8]>),
}

impl PasteContent {
    /// Borrows the exact bytes transferred by the artifact client.
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Png(bytes) => bytes,
        }
    }

    /// Tests retained-source identity for explicit upload retries.
    pub fn same_source(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Text(a), Self::Text(b)) => Arc::ptr_eq(a, b),
            (Self::Png(a), Self::Png(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// One unfinished unsolicited MIME offer or correlated content read.
struct Transfer {
    /// None while receiving the user gesture's MIME inventory.
    id: Option<String>,
    /// Exact requested MIME, or dot while receiving the inventory.
    mime: String,
    /// Location and optional single-use grant from the opening offer.
    authority: String,
    /// Exact grant from the opening offer; repeated DATA grants must agree.
    grant: Option<String>,
    /// Independently decoded chunks; never published before DONE.
    bytes: Vec<u8>,
    /// Whether the correlated opening OK has been received.
    opened: bool,
    /// Fixed total deadline; trickle traffic cannot extend it.
    deadline: Instant,
}

/// Sans-I/O clipboard owner shared by real and injected terminal events.
pub(super) struct ClipboardPaste {
    /// Per-owner identity prevents replies from a previous attachment matching.
    identity: String,
    /// True only after a supported mode report for the current ownership epoch.
    enabled: bool,
    /// Attachment-lifetime obligation; cancellation cannot retract queued
    /// bytes.
    stream_obligation: bool,
    /// Failed handoffs keep native mode disabled until a successful explicit
    /// retry.
    handoff_blocked: bool,
    /// Deadline for a nonblocking capability probe.
    probe: Option<Instant>,
    /// Unique read serial retained across resets to isolate late replies.
    serial: u64,
    /// At most one offer/read can own the draft.
    transfer: Option<Transfer>,
}

impl Default for ClipboardPaste {
    fn default() -> Self {
        static OWNER: AtomicU64 = AtomicU64::new(0);
        let serial = OWNER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            identity: format!("{}-{nanos}-{serial}", std::process::id()),
            enabled: false,
            stream_obligation: false,
            handoff_blocked: false,
            probe: None,
            serial: 0,
            transfer: None,
        }
    }
}

/// Effects applied by the existing terminal input/output owner.
#[derive(Default)]
pub(super) struct Effects {
    /// Control bytes to write atomically; never logged.
    pub(super) output: Vec<u8>,
    /// Fully completed content, never a partial transfer.
    pub(super) content: Option<PasteContent>,
    /// Content-free diagnostic for the interactive user.
    pub(super) notice: Option<String>,
}

impl ClipboardPaste {
    /// Starts a fresh, bounded capability probe without waiting at startup.
    pub(super) fn probe(&mut self, now: Instant) {
        self.enabled = false;
        self.transfer = None;
        self.probe = Some(now + Duration::from_secs(1));
    }

    /// Revokes local ownership; already queued terminal bytes cannot be
    /// retracted.
    pub(super) fn reset(&mut self) {
        self.enabled = false;
        self.probe = None;
        self.transfer = None;
    }

    /// Admits supported/reset or supported/set reports only during a live
    /// probe.
    pub(super) fn mode_report(&mut self, supported: bool, now: Instant) -> Effects {
        let admitted = self.probe.take().is_some_and(|deadline| now < deadline);
        if supported && admitted && !self.handoff_blocked {
            self.stream_obligation = true;
            self.enabled = true;
            Effects {
                output: b"\x1b[?5522h".to_vec(),
                ..Effects::default()
            }
        } else {
            Effects::default()
        }
    }

    /// Requires a stream fence only after native mode was actually admitted.
    pub(super) fn needs_fence(&self) -> bool {
        self.stream_obligation
    }

    /// Revokes acquisition while retaining the independent stream obligation.
    pub(super) fn begin_handoff(&mut self) -> ClipboardFence {
        self.reset();
        self.handoff_blocked = true;
        self.serial += 1;
        ClipboardFence::new(format!("tau-fence-{}-{}", self.identity, self.serial))
    }

    /// Discharges the obligation only after the correlated fence completed.
    pub(super) fn finish_handoff(&mut self) {
        self.stream_obligation = false;
        self.handoff_blocked = false;
    }

    /// Indicates acquisition ownership, separate from artifact upload
    /// ownership.
    pub(super) fn busy(&self) -> bool {
        self.transfer.is_some()
    }

    /// Earliest timer the input owner must service even without incoming bytes.
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.transfer.as_ref().map(|t| t.deadline).or(self.probe)
    }

    /// Discards timed-out acquisition without touching the draft or retrying
    /// reads.
    pub(super) fn expire(&mut self, now: Instant) -> Effects {
        if self.probe.is_some_and(|deadline| deadline <= now) {
            self.probe = None;
        }
        if self.transfer.as_ref().is_some_and(|t| now >= t.deadline) {
            self.transfer = None;
            return Self::failure(
                "Clipboard paste timed out; draft unchanged. Paste again to retry.",
            );
        }
        Effects::default()
    }

    /// Consumes only OSC5522 read responses and user-gesture offers.
    pub(super) fn receive(&mut self, body: &[u8], now: Instant, upload_busy: bool) -> Effects {
        let Ok(body) = std::str::from_utf8(body) else {
            return Effects::default();
        };
        let Some(body) = body.strip_prefix("5522;") else {
            return Effects::default();
        };
        // WezTerm emits metadata-only OK, DONE and error frames without a
        // payload separator; DATA still requires one, even for empty bytes.
        let (metadata, payload) = body.split_once(';').unwrap_or((body, ""));
        let mut fields = HashMap::new();
        for field in metadata.split(':') {
            let Some((key, value)) = field.split_once('=') else {
                return Effects::default();
            };
            if fields.insert(key, value).is_some() {
                return Effects::default();
            }
        }
        if fields.get("type") != Some(&"read") {
            return Effects::default();
        }
        let Some(status) = fields.get("status").copied() else {
            return Effects::default();
        };
        let id = fields.get("id").copied();
        if id.is_none() && status == "OK" && self.enabled {
            return self.open_offer(&fields, now, upload_busy);
        }
        let Some(transfer) = self.transfer.as_mut() else {
            return Effects::default();
        };
        if id != transfer.id.as_deref() {
            return Effects::default();
        }
        if now >= transfer.deadline {
            return self.expire(now);
        }
        if transfer.id.is_none()
            && fields
                .get("pw")
                .is_some_and(|pw| Some(*pw) != transfer.grant.as_deref())
        {
            self.transfer = None;
            return Self::failure("Clipboard paste grant changed; draft unchanged.");
        }
        match status {
            "OK" if transfer.id.is_some() && !transfer.opened => transfer.opened = true,
            "DATA" if transfer.opened => {
                if !body.contains(';') {
                    self.transfer = None;
                    return Self::failure(
                        "Missing clipboard DATA payload separator; draft unchanged.",
                    );
                }
                if let Err(error) = Self::append_data(transfer, &fields, payload) {
                    self.transfer = None;
                    return Self::failure(error);
                }
            }
            "DONE" if transfer.opened => {
                let transfer = self.transfer.take().expect("matched transfer");
                return self.complete(transfer, now);
            }
            "EPERM" | "EBUSY" | "ENOSYS" if transfer.id.is_some() && !transfer.opened => {
                self.transfer = None;
                return Self::failure(match status {
                    "EPERM" => "Clipboard read permission denied; draft unchanged.",
                    "EBUSY" => "Clipboard read busy; paste again to retry.",
                    _ => "Clipboard location unavailable; draft unchanged.",
                });
            }
            _ => {
                self.transfer = None;
                return Self::failure("Invalid clipboard response order; draft unchanged.");
            }
        }
        Effects::default()
    }

    /// Admits one user gesture without mixing overlapping unidentified offers.
    fn open_offer(
        &mut self,
        fields: &HashMap<&str, &str>,
        now: Instant,
        upload_busy: bool,
    ) -> Effects {
        if self.busy() || upload_busy {
            if self
                .transfer
                .as_ref()
                .is_some_and(|transfer| transfer.id.is_none())
            {
                self.transfer = None;
                return Self::failure(
                    "Overlapping clipboard offers discarded; draft unchanged. Paste again.",
                );
            }
            return Self::failure("Paste busy; wait or press Ctrl-C before pasting again.");
        }
        let mut authority = String::new();
        if let Some(location) = fields.get("loc") {
            if *location != "primary" {
                return Self::failure("Unsupported clipboard location.");
            }
            authority.push_str(":loc=primary");
        }
        if let Some(password) = fields.get("pw") {
            if password.len() > 4096 || STANDARD.decode(password).is_err() {
                return Self::failure("Invalid clipboard paste grant.");
            }
            authority.push_str(":pw=");
            authority.push_str(password);
        }
        self.transfer = Some(Transfer {
            id: None,
            mime: ".".into(),
            authority,
            grant: fields.get("pw").map(|password| (*password).to_owned()),
            bytes: Vec::new(),
            opened: true,
            deadline: now + Duration::from_secs(10),
        });
        Effects::default()
    }

    /// Decodes each independently padded chunk before bounded concatenation.
    fn append_data(
        transfer: &mut Transfer,
        fields: &HashMap<&str, &str>,
        payload: &str,
    ) -> Result<(), &'static str> {
        let valid = fields
            .get("mime")
            .and_then(|mime| STANDARD.decode(mime).ok())
            .is_some_and(|mime| mime == transfer.mime.as_bytes());
        let decoded = (payload.len() <= 5464)
            .then(|| STANDARD.decode(payload))
            .transpose();
        let Ok(Some(decoded)) = decoded else {
            return Err("Invalid clipboard paste encoding; draft unchanged.");
        };
        if !valid || decoded.len() > MAX_CHUNK {
            return Err("Invalid clipboard MIME or chunk size; draft unchanged.");
        }
        let inventory = transfer.id.is_none();
        let limit = if inventory { 64 * 1024 } else { MAX_BYTES };
        if transfer.bytes.len() + decoded.len() > limit {
            return Err(if inventory {
                "Clipboard MIME inventory exceeds 64 KiB; draft unchanged."
            } else {
                "Clipboard paste exceeds 16 MiB; draft unchanged."
            });
        }
        transfer.bytes.extend(decoded);
        Ok(())
    }

    /// DONE either selects one offered MIME or releases complete content.
    fn complete(&mut self, transfer: Transfer, now: Instant) -> Effects {
        if transfer.id.is_none() {
            return self.request_content(transfer, now);
        }
        let content = if transfer.mime == "image/png" {
            PasteContent::Png(transfer.bytes.into())
        } else {
            let Ok(text) = String::from_utf8(transfer.bytes) else {
                return Self::failure("Clipboard text is not UTF-8; draft unchanged.");
            };
            PasteContent::Text(text.into())
        };
        Effects {
            content: Some(content),
            ..Effects::default()
        }
    }

    /// Requests only one supported MIME using the original gesture's authority.
    fn request_content(&mut self, transfer: Transfer, now: Instant) -> Effects {
        let Ok(inventory) = std::str::from_utf8(&transfer.bytes) else {
            return Self::failure("Invalid clipboard MIME inventory.");
        };
        let mimes: Vec<_> = inventory.split_ascii_whitespace().collect();
        let mime = ["image/png", "text/plain;charset=utf-8", "text/plain"]
            .into_iter()
            .find(|mime| mimes.contains(mime));
        let Some(mime) = mime else {
            return Self::failure(
                "Clipboard has no supported PNG or UTF-8 plain text. File URIs are not uploaded.",
            );
        };
        self.serial += 1;
        let id = format!("tau-paste-{}-{}", self.identity, self.serial);
        let output = format!(
            "\x1b]5522;type=read:id={id}{}:name={};{}\x1b\\",
            transfer.authority,
            STANDARD.encode("Paste event"),
            STANDARD.encode(mime)
        )
        .into_bytes();
        self.transfer = Some(Transfer {
            id: Some(id),
            mime: mime.into(),
            authority: String::new(),
            grant: None,
            bytes: Vec::new(),
            opened: false,
            deadline: now + Duration::from_secs(15),
        });
        Effects {
            output,
            ..Effects::default()
        }
    }

    /// Produces a bounded diagnostic containing no terminal-supplied text.
    fn failure(message: &str) -> Effects {
        Effects {
            notice: Some(message.into()),
            ..Effects::default()
        }
    }
}
