//! Best-effort, process-local mirroring of supervised extension stderr.

use std::fmt::Write as _;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use tau_proto::ExtensionName;

/// Private process-local mirror queue capacity.
const MIRROR_QUEUE_CAPACITY: usize = 256;
/// Maximum ordinary raw payload bytes in one mirror record.
const MAX_RECORD_BYTES: usize = 4096;

/// Immutable child identity attached to every mirrored stderr record.
#[derive(Clone)]
pub(crate) struct ExtensionStderrIdentity {
    /// Validated configured extension name.
    extension: ExtensionName,
    /// Harness restart generation, starting at zero for the initial child.
    generation: u32,
    /// Operating-system child process identifier.
    pid: u32,
}

impl ExtensionStderrIdentity {
    /// Builds the immutable identity for one supervised child.
    pub(crate) fn new(extension: ExtensionName, generation: u32, pid: u32) -> Self {
        Self {
            extension,
            generation,
            pid,
        }
    }
}

/// Cloneable admission handle for the single process-wide stderr mirror worker.
#[derive(Clone)]
pub(crate) struct ExtensionStderrMirror {
    /// Bounded nonblocking admission channel.
    sender: mpsc::SyncSender<Vec<u8>>,
    /// Process-wide permanent disable bit set after the first sink failure.
    disabled: Arc<AtomicBool>,
}

impl ExtensionStderrMirror {
    /// Starts the production worker on an independent duplicate of inherited
    /// process stderr, or disables mirroring when duplication fails.
    pub(crate) fn stderr() -> Option<Self> {
        let stderr = std::io::stderr();
        Self::from_stderr_duplicate(rustix::io::dup(stderr.as_fd()))
    }

    /// Continues mirror setup only after inherited-stderr duplication succeeds.
    fn from_stderr_duplicate(duplicated: rustix::io::Result<rustix::fd::OwnedFd>) -> Option<Self> {
        let duplicated = duplicated.ok()?;
        Self::try_with_writer_and_spawner(File::from(duplicated), MIRROR_QUEUE_CAPACITY, |task| {
            thread::Builder::new()
                .name("tau-extension-stderr-mirror".to_owned())
                .spawn(task)
                .map(drop)
        })
    }

    /// Starts a mirror worker with an injected sink and private queue capacity.
    #[cfg(test)]
    pub(crate) fn with_writer_and_capacity(
        writer: impl Write + Send + 'static,
        capacity: usize,
    ) -> Self {
        Self::try_with_writer_and_spawner(writer, capacity, |task| {
            thread::Builder::new()
                .name("tau-extension-stderr-mirror-test".to_owned())
                .spawn(task)
                .map(drop)
        })
        .expect("test mirror worker must spawn")
    }

    /// Performs fallible worker setup without affecting harness lifecycle.
    fn try_with_writer_and_spawner(
        writer: impl Write + Send + 'static,
        capacity: usize,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
    ) -> Option<Self> {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let disabled = Arc::new(AtomicBool::new(false));
        let worker_disabled = disabled.clone();
        spawn(Box::new(move || {
            mirror_worker(receiver, writer, worker_disabled);
        }))
        .ok()?;
        Some(Self { sender, disabled })
    }

    /// Creates per-child streaming state with independent loss accounting.
    pub(crate) fn logger(&self, identity: ExtensionStderrIdentity) -> ExtensionStderrLogger {
        ExtensionStderrLogger {
            mirror: self.clone(),
            identity,
            framer: StderrFramer::default(),
            dropped_records: 0,
            dropped_raw_bytes: 0,
            enabled: true,
        }
    }

    /// Attempts to admit one complete rendered record without blocking.
    fn try_send(&self, record: Vec<u8>) -> Result<(), MirrorAdmissionError> {
        if self.disabled.load(Ordering::Acquire) {
            return Err(MirrorAdmissionError::Disabled);
        }
        match self.sender.try_send(record) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => Err(MirrorAdmissionError::Full),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.disabled.store(true, Ordering::Release);
                Err(MirrorAdmissionError::Disabled)
            }
        }
    }
}

/// Per-child streaming framer and dropped-record accounting.
pub(crate) struct ExtensionStderrLogger {
    /// Process-wide queue handle.
    mirror: ExtensionStderrMirror,
    /// Immutable attribution for this child generation.
    identity: ExtensionStderrIdentity,
    /// Streaming LF and UTF-8-aware record framer.
    framer: StderrFramer,
    /// Mirror records dropped since the last admitted notice.
    dropped_records: u64,
    /// Raw payload bytes represented by dropped records.
    dropped_raw_bytes: u64,
    /// Whether raw-file failure permanently disabled this logger.
    enabled: bool,
}

impl ExtensionStderrLogger {
    /// Frames bytes whose corresponding private-file write and flush succeeded.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        if !self.enabled {
            return;
        }
        let records = self.framer.feed(bytes);
        for record in records {
            self.admit(record);
        }
    }

    /// Emits the final unterminated suffix after private-file draining
    /// completes.
    pub(crate) fn finish(&mut self) {
        if !self.enabled {
            return;
        }
        for record in self.framer.finish() {
            self.admit(record);
        }
    }

    /// Permanently stops this logger after its required private sink fails.
    pub(crate) fn disable(&mut self) {
        self.enabled = false;
        self.framer.clear();
    }

    /// Admits a later content record, first reporting any accumulated loss.
    fn admit(&mut self, record: FramedRecord) {
        let raw_len = record.source_len();
        let rendered = render_record(&self.identity, record);
        if self.dropped_records != 0 {
            let mut recovered =
                render_dropped(&self.identity, self.dropped_records, self.dropped_raw_bytes);
            recovered.extend_from_slice(&rendered);
            match self.mirror.try_send(recovered) {
                Ok(()) => {
                    self.dropped_records = 0;
                    self.dropped_raw_bytes = 0;
                    return;
                }
                Err(MirrorAdmissionError::Full) => {
                    self.record_drop(raw_len);
                    return;
                }
                Err(MirrorAdmissionError::Disabled) => {
                    self.enabled = false;
                    return;
                }
            }
        }
        match self.mirror.try_send(rendered) {
            Ok(()) => {}
            Err(MirrorAdmissionError::Full) => self.record_drop(raw_len),
            Err(MirrorAdmissionError::Disabled) => self.enabled = false,
        }
    }

    /// Adds one dropped content record using saturating diagnostic counters.
    fn record_drop(&mut self, raw_len: usize) {
        self.dropped_records = self.dropped_records.saturating_add(1);
        self.dropped_raw_bytes = self.dropped_raw_bytes.saturating_add(raw_len as u64);
    }
}

/// Nonblocking queue admission outcome.
enum MirrorAdmissionError {
    /// The bounded queue has no free slot.
    Full,
    /// The process-wide sink has failed or its worker has exited.
    Disabled,
}

/// Boundary assigned to one escaped record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordBoundary {
    /// LF terminated the logical line.
    Line,
    /// A logical line exceeded the bounded fragment size.
    Chunk,
    /// Child stderr ended with an unterminated suffix.
    Eof,
}

impl RecordBoundary {
    /// Returns the canonical lowercase wire spelling.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Chunk => "chunk",
            Self::Eof => "eof",
        }
    }
}

/// One framed raw payload before canonical escaping.
struct FramedRecord {
    /// Boundary semantics for the payload.
    boundary: RecordBoundary,
    /// Raw payload bytes, excluding the splitting LF.
    raw: Vec<u8>,
}

impl FramedRecord {
    /// Counts original child bytes represented by this record, including a
    /// splitting LF even though the rendered message omits it.
    fn source_len(&self) -> usize {
        self.raw.len() + usize::from(self.boundary == RecordBoundary::Line)
    }
}

/// Streaming LF splitter with bounded UTF-8-aware fragments.
#[derive(Default)]
struct StderrFramer {
    /// Unemitted bytes from the current logical line.
    pending: Vec<u8>,
    /// Whether the current logical line already emitted at least one chunk.
    line_chunked: bool,
}

impl StderrFramer {
    /// Adds bytes and returns every now-complete record.
    fn feed(&mut self, bytes: &[u8]) -> Vec<FramedRecord> {
        self.pending.extend_from_slice(bytes);
        let mut records = Vec::new();
        loop {
            if let Some(lf) = self.pending.iter().position(|byte| *byte == b'\n') {
                if lf <= MAX_RECORD_BYTES {
                    let mut suffix = self.pending.split_off(lf + 1);
                    std::mem::swap(&mut suffix, &mut self.pending);
                    suffix.pop();
                    records.push(FramedRecord {
                        boundary: RecordBoundary::Line,
                        raw: suffix,
                    });
                    self.line_chunked = false;
                    continue;
                }
                let split = utf8_aware_split(&self.pending[..lf], true)
                    .expect("bytes before an observed LF never need lookahead");
                let suffix = self.pending.split_off(split);
                records.push(FramedRecord {
                    boundary: RecordBoundary::Chunk,
                    raw: std::mem::replace(&mut self.pending, suffix),
                });
                self.line_chunked = true;
                continue;
            }
            let Some(split) = utf8_aware_split(&self.pending, false) else {
                break;
            };
            if MAX_RECORD_BYTES < split || split < self.pending.len() {
                let suffix = self.pending.split_off(split);
                records.push(FramedRecord {
                    boundary: RecordBoundary::Chunk,
                    raw: std::mem::replace(&mut self.pending, suffix),
                });
                self.line_chunked = true;
            } else {
                break;
            }
        }
        records
    }

    /// Returns the final unterminated payload, if any.
    fn finish(&mut self) -> Vec<FramedRecord> {
        let mut records = Vec::new();
        while self.pending.len() > MAX_RECORD_BYTES {
            let split = utf8_aware_split(&self.pending, true)
                .expect("EOF framing never waits for more bytes");
            let suffix = self.pending.split_off(split);
            records.push(FramedRecord {
                boundary: RecordBoundary::Chunk,
                raw: std::mem::replace(&mut self.pending, suffix),
            });
        }
        if !self.pending.is_empty() || self.line_chunked {
            records.push(FramedRecord {
                boundary: RecordBoundary::Eof,
                raw: std::mem::take(&mut self.pending),
            });
        }
        self.line_chunked = false;
        records
    }

    /// Discards unmirrored state after the authoritative raw sink fails.
    fn clear(&mut self) {
        self.pending.clear();
        self.line_chunked = false;
    }
}

/// Chooses 4096 bytes unless extending by at most three bytes avoids splitting
/// a valid UTF-8 scalar.
fn utf8_aware_split(bytes: &[u8], eof: bool) -> Option<usize> {
    if bytes.len() <= MAX_RECORD_BYTES {
        return Some(bytes.len());
    }
    let mut lead = MAX_RECORD_BYTES;
    while lead > MAX_RECORD_BYTES.saturating_sub(3)
        && bytes
            .get(lead)
            .is_some_and(|byte| byte & 0b1100_0000 == 0b1000_0000)
    {
        lead -= 1;
    }
    let Some(width) = utf8_width(bytes[lead]) else {
        return Some(MAX_RECORD_BYTES);
    };
    let end = lead + width;
    if lead < MAX_RECORD_BYTES && end > bytes.len() {
        let available_tail = &bytes[lead + 1..];
        if !eof
            && available_tail
                .iter()
                .all(|byte| byte & 0b1100_0000 == 0b1000_0000)
        {
            return None;
        }
        return Some(MAX_RECORD_BYTES);
    }
    if lead < MAX_RECORD_BYTES
        && MAX_RECORD_BYTES < end
        && end <= bytes.len()
        && std::str::from_utf8(&bytes[lead..end]).is_ok()
    {
        Some(end)
    } else {
        Some(MAX_RECORD_BYTES)
    }
}

/// Returns the encoded width for a possible UTF-8 leading byte.
fn utf8_width(byte: u8) -> Option<usize> {
    match byte {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

/// Renders one ordinary canonical mirror record.
fn render_record(identity: &ExtensionStderrIdentity, record: FramedRecord) -> Vec<u8> {
    let mut output = record_prefix(identity, record.boundary.as_str());
    escape_bytes(&record.raw, &mut output);
    output.push_str("\"\n");
    output.into_bytes()
}

/// Renders one bounded loss notice before later content.
fn render_dropped(identity: &ExtensionStderrIdentity, records: u64, raw_bytes: u64) -> Vec<u8> {
    let mut output = record_prefix(identity, "dropped");
    writeln!(output, "records={records} raw_bytes={raw_bytes}\"")
        .expect("writing to a String cannot fail");
    output.into_bytes()
}

/// Builds the common trusted attribution prefix.
fn record_prefix(identity: &ExtensionStderrIdentity, boundary: &str) -> String {
    format!(
        "tau: extension stderr: extension={} generation={} pid={} boundary={} message=\"",
        identity.extension, identity.generation, identity.pid, boundary
    )
}

/// Applies the canonical escaping contract to arbitrary child bytes.
fn escape_bytes(mut bytes: &[u8], output: &mut String) {
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                escape_valid_text(text, output);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                escape_valid_text(
                    std::str::from_utf8(&bytes[..valid])
                        .expect("Utf8Error::valid_up_to prefix must be valid"),
                    output,
                );
                let invalid_len = error.error_len().unwrap_or(bytes.len() - valid);
                for byte in &bytes[valid..valid + invalid_len] {
                    write!(output, "\\x{byte:02X}").expect("writing to a String cannot fail");
                }
                bytes = &bytes[valid + invalid_len..];
            }
        }
    }
}

/// Escapes controls and direction-changing Unicode while preserving printable
/// text.
fn escape_valid_text(text: &str, output: &mut String) {
    for character in text.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\t' => output.push_str("\\t"),
            '\r' => output.push_str("\\r"),
            '\u{2028}'
            | '\u{2029}'
            | '\u{061C}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}' => {
                write!(output, "\\u{{{:X}}}", character as u32)
                    .expect("writing to a String cannot fail");
            }
            '\u{0000}'..='\u{0008}' | '\u{000B}'..='\u{001F}' | '\u{007F}'..='\u{009F}' => {
                for byte in character.to_string().as_bytes() {
                    write!(output, "\\x{byte:02X}").expect("writing to a String cannot fail");
                }
            }
            _ => output.push(character),
        }
    }
}

/// Serializes complete records and permanently disables the mirror on sink
/// error.
fn mirror_worker(
    receiver: mpsc::Receiver<Vec<u8>>,
    mut writer: impl Write,
    disabled: Arc<AtomicBool>,
) {
    while let Ok(record) = receiver.recv() {
        if writer
            .write_all(&record)
            .and_then(|()| writer.flush())
            .is_err()
        {
            disabled.store(true, Ordering::Release);
            break;
        }
    }
}

#[cfg(test)]
mod tests;
