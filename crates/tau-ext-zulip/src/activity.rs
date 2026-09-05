//! Bounded process-local summaries of relevant non-allowlisted Zulip activity.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// Maximum active stream/topic scopes retained by one extension process.
const MAX_BUCKETS: usize = 64;
/// Maximum distinct native senders retained in one conversation scope.
const MAX_SENDERS_PER_BUCKET: usize = 32;
/// Largest presented count before the summary marks it as saturated.
const MAX_PRESENTED_COUNT: u16 = 9_999;
/// Maximum input Unicode scalar count inspected for a display label.
const MAX_LABEL_INPUT_SCALARS: usize = 48;
/// Maximum input UTF-8 byte count inspected for a display label.
const MAX_LABEL_INPUT_BYTES: usize = 128;
/// Maximum retained sanitized display-label bytes.
const MAX_LABEL_OUTPUT_BYTES: usize = 96;
/// Marker appended after truncating a sanitized display label.
const LABEL_TRUNCATION_MARKER: &str = "...";
/// Maximum bridge-authored summary size.
pub(crate) const MAX_ACTIVITY_NOTE_BYTES: usize = 4_096;
/// Maximum age of an unflushed conversation bucket.
const BUCKET_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
/// Fixed XML-lite opening tag for one external activity summary.
const ACTIVITY_SUMMARY_OPENING: &str = "<activity_summary content_trust=\"external\">\n";
/// Fixed XML-lite closing tag separating the summary from exact admitted
/// Markdown.
const ACTIVITY_SUMMARY_CLOSING: &str = "</activity_summary>\n\n";

/// Bounded accumulator for relevant stream messages rejected only by sender
/// policy.
#[derive(Default)]
pub(crate) struct ActivityAccumulator {
    /// Conversation buckets indexed by opaque publisher-domain stable ID.
    buckets: HashMap<String, ActivityBucket>,
    /// Saturating count for messages whose new conversation bucket could not
    /// fit.
    untracked_route_messages: u16,
}

/// One removable conversation snapshot used for checked report composition.
pub(crate) struct ActivitySnapshot {
    /// Opaque conversation key used to restore a failed submission.
    conversation_id: String,
    /// Retained bounded sender activity.
    bucket: ActivityBucket,
}

/// Bounded activity retained for one exact native stream/topic conversation.
struct ActivityBucket {
    /// First observation time, which owns the fixed lifetime.
    first_seen: Instant,
    /// Last observation time for local diagnostics and deterministic tests.
    last_seen: Instant,
    /// Sender entries indexed only by private numeric Zulip identity.
    senders: HashMap<u64, SenderActivity>,
    /// Messages from additional senders omitted after sender capacity is full.
    other_sender_messages: u16,
}

/// Retained summary data for one stable native sender.
struct SenderActivity {
    /// Saturating relevant-message count.
    count: u16,
    /// Latest bounded and structurally escaped display hint.
    label: Option<String>,
    /// Whether the retained display hint changed during this bucket lifetime.
    label_changed: bool,
    /// Full route-scoped keyed digest used to render collision-safe pseudonyms.
    pseudonym_digest: [u8; 32],
}

impl ActivityAccumulator {
    /// Record one otherwise-admissible stream message rejected only by sender
    /// policy.
    pub(crate) fn observe(
        &mut self,
        conversation_id: &str,
        sender_id: u64,
        sender_full_name: Option<&str>,
        id_key: &[u8; 32],
        now: Instant,
    ) {
        self.prune_expired(now);
        let label = sender_full_name.and_then(sanitize_label);
        if let Some(bucket) = self.buckets.get_mut(conversation_id) {
            bucket.observe(conversation_id, sender_id, label, id_key, now);
            return;
        }
        if MAX_BUCKETS <= self.buckets.len() {
            increment(&mut self.untracked_route_messages);
            return;
        }
        let mut bucket = ActivityBucket {
            first_seen: now,
            last_seen: now,
            senders: HashMap::new(),
            other_sender_messages: 0,
        };
        bucket.observe(conversation_id, sender_id, label, id_key, now);
        self.buckets.insert(conversation_id.to_owned(), bucket);
    }

    /// Remove one same-scope bucket for possible attachment to an admitted
    /// message.
    pub(crate) fn take(&mut self, conversation_id: &str, now: Instant) -> Option<ActivitySnapshot> {
        self.prune_expired(now);
        self.buckets
            .remove(conversation_id)
            .map(|bucket| ActivitySnapshot {
                conversation_id: conversation_id.to_owned(),
                bucket,
            })
    }

    /// Restore a snapshot after checked report submission fails.
    pub(crate) fn restore(&mut self, snapshot: ActivitySnapshot) {
        self.buckets
            .entry(snapshot.conversation_id)
            .or_insert(snapshot.bucket);
    }

    /// Discard every process-local observation after an authority epoch
    /// changes.
    pub(crate) fn clear(&mut self) {
        self.buckets.clear();
        self.untracked_route_messages = 0;
    }

    /// Drop buckets whose fixed first-observation lifetime has elapsed.
    pub(crate) fn prune_expired(&mut self, now: Instant) {
        self.buckets
            .retain(|_, bucket| now.duration_since(bucket.first_seen) < BUCKET_LIFETIME);
    }

    #[cfg(test)]
    /// Return the number of retained conversation scopes for focused tests.
    pub(crate) fn bucket_count(&self) -> usize {
        self.buckets.len()
    }
}

impl ActivityBucket {
    /// Update one sender or the bounded overflow counter.
    fn observe(
        &mut self,
        conversation_id: &str,
        sender_id: u64,
        label: Option<String>,
        id_key: &[u8; 32],
        now: Instant,
    ) {
        self.last_seen = now;
        if let Some(sender) = self.senders.get_mut(&sender_id) {
            increment(&mut sender.count);
            if sender.label != label {
                sender.label = label;
                sender.label_changed = true;
            }
            return;
        }
        if MAX_SENDERS_PER_BUCKET <= self.senders.len() {
            increment(&mut self.other_sender_messages);
            return;
        }
        self.senders.insert(
            sender_id,
            SenderActivity {
                count: 1,
                label,
                label_changed: false,
                pseudonym_digest: sender_pseudonym(id_key, conversation_id, sender_id),
            },
        );
    }
}

impl ActivitySnapshot {
    /// Render one complete bridge-authored note within the caller's byte
    /// budget.
    pub(crate) fn render(&self, max_bytes: usize) -> Option<String> {
        let mut senders = self.bucket.senders.values().collect::<Vec<_>>();
        senders.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.pseudonym_digest.cmp(&right.pseudonym_digest))
        });
        let colliding_prefixes = colliding_pseudonym_prefixes(&senders);
        let mut rendered_lines = senders
            .iter()
            .map(|sender| render_sender(sender, &colliding_prefixes))
            .collect::<Vec<_>>();
        let mut omitted_messages = self.bucket.other_sender_messages;
        loop {
            let overflow_line = (omitted_messages != 0).then(|| {
                format!(
                    "- {} additional messages from senders not individually listed\n",
                    display_count(omitted_messages)
                )
            });
            let size = ACTIVITY_SUMMARY_OPENING.len()
                + rendered_lines.iter().map(String::len).sum::<usize>()
                + overflow_line.as_ref().map_or(0, String::len)
                + ACTIVITY_SUMMARY_CLOSING.len();
            if size <= max_bytes && (!rendered_lines.is_empty() || overflow_line.is_some()) {
                let mut output = String::with_capacity(size);
                output.push_str(ACTIVITY_SUMMARY_OPENING);
                for line in rendered_lines {
                    output.push_str(&line);
                }
                if let Some(line) = overflow_line {
                    output.push_str(&line);
                }
                output.push_str(ACTIVITY_SUMMARY_CLOSING);
                return Some(output);
            }
            let removed = rendered_lines.pop()?;
            let removed_sender = senders
                .get(rendered_lines.len())
                .expect("rendered sender line has matching entry");
            let _ = removed;
            omitted_messages = omitted_messages
                .saturating_add(removed_sender.count)
                .min(MAX_PRESENTED_COUNT);
        }
    }
}

/// Increment one bounded presentation counter.
fn increment(value: &mut u16) {
    *value = value.saturating_add(1).min(MAX_PRESENTED_COUNT);
}

/// Derive one route-scoped, non-reversible sender pseudonym.
fn sender_pseudonym(id_key: &[u8; 32], conversation_id: &str, sender_id: u64) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_keyed(id_key);
    hasher.update(b"tau-ext-zulip/non-allowlisted-sender/v1\0");
    hasher.update(conversation_id.as_bytes());
    hasher.update(&sender_id.to_le_bytes());
    *hasher.finalize().as_bytes()
}

/// Find abbreviated digest prefixes that collide within one retained bucket.
fn colliding_pseudonym_prefixes(senders: &[&SenderActivity]) -> HashMap<[u8; 8], usize> {
    let mut counts = HashMap::new();
    for sender in senders {
        let prefix: [u8; 8] = sender.pseudonym_digest[..8]
            .try_into()
            .expect("fixed digest prefix");
        *counts.entry(prefix).or_insert(0) += 1;
    }
    counts
}

/// Render one structurally inert sender line.
fn render_sender(sender: &SenderActivity, prefixes: &HashMap<[u8; 8], usize>) -> String {
    let prefix: [u8; 8] = sender.pseudonym_digest[..8]
        .try_into()
        .expect("fixed digest prefix");
    let digest_bytes = if prefixes.get(&prefix) == Some(&1) {
        &sender.pseudonym_digest[..8]
    } else {
        &sender.pseudonym_digest[..]
    };
    let mut line = String::from("- ");
    if let Some(label) = &sender.label {
        line.push('"');
        line.push_str(label);
        line.push_str("\" ");
    }
    line.push_str("(sender-");
    for byte in digest_bytes {
        let _ = write!(line, "{byte:02x}");
    }
    line.push_str("): ");
    line.push_str(&display_count(sender.count));
    line.push_str(if sender.count == 1 {
        " message"
    } else {
        " messages"
    });
    if sender.label_changed {
        line.push_str(" (name changed)");
    }
    line.push('\n');
    line
}

/// Render a bounded count, marking its saturated upper edge.
fn display_count(count: u16) -> String {
    if count == MAX_PRESENTED_COUNT {
        format!("{MAX_PRESENTED_COUNT}+")
    } else {
        count.to_string()
    }
}

/// Sanitize an attacker-controlled display hint into one bounded quoted datum.
fn sanitize_label(value: &str) -> Option<String> {
    let mut bounded = String::new();
    for character in value.chars().take(MAX_LABEL_INPUT_SCALARS) {
        if MAX_LABEL_INPUT_BYTES < bounded.len() + character.len_utf8() {
            break;
        }
        bounded.push(character);
    }
    let bounded = bounded.trim();
    if bounded.is_empty() {
        return None;
    }
    let mut output = String::new();
    let mut unit_starts = Vec::new();
    for character in bounded.chars() {
        let mut unit = String::new();
        if tau_proto::requires_visible_escape(character)
            || matches!(
                character,
                '\\' | '"' | '<' | '>' | '&' | '`' | '[' | ']' | '{' | '}'
            )
        {
            let _ = write!(unit, "\\u{{{:04X}}}", character as u32);
        } else {
            unit.push(character);
        }
        if output.len() + unit.len() <= MAX_LABEL_OUTPUT_BYTES {
            unit_starts.push(output.len());
            output.push_str(&unit);
            continue;
        }
        while MAX_LABEL_OUTPUT_BYTES < output.len() + LABEL_TRUNCATION_MARKER.len() {
            let start = unit_starts
                .pop()
                .expect("label output bound exceeds every escaped unit");
            output.truncate(start);
        }
        output.push_str(LABEL_TRUNCATION_MARKER);
        break;
    }
    (!output.is_empty()).then_some(output)
}

#[cfg(test)]
mod tests;
