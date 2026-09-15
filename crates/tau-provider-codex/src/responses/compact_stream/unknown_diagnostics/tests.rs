use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use super::super::CompactItemPhase;
use super::*;

/// In-memory normal-log capture for privacy and flood-bound assertions.
#[derive(Clone, Default)]
struct TraceWriter {
    /// Captured formatted records shared by subscriber clones.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for TraceWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes
            .lock()
            .expect("trace lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Presence is discoverable at normal levels, duplicate and distinct floods are
/// bounded, and malformed provider labels never leak into ordinary logs.
#[test]
fn unknown_diagnostics_are_searchable_bounded_and_sanitized() {
    let writer = TraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer({
            let writer = writer.clone();
            move || writer.clone()
        })
        .finish();
    let mut diagnostics = UnknownDiagnostics::default();
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..100 {
            diagnostics.observe(
                "ap-test",
                CompactItemPhase::Added,
                "event_type",
                Some("response.notification.future"),
                "ignored_informational",
            );
        }
        diagnostics.observe(
            "ap-test",
            CompactItemPhase::Added,
            "output_item_type",
            Some("private\nsecret"),
            "rejected_output_item",
        );
        diagnostics.observe(
            "ap-test",
            CompactItemPhase::Added,
            "event_type",
            Some("response.unknown"),
            "rejected_unknown_shape",
        );
        for n in 0..100 {
            diagnostics.observe(
                "ap-test",
                CompactItemPhase::Added,
                "event_type",
                Some(&format!("response.notification.future{n}")),
                "ignored_informational",
            );
        }
    });
    let text =
        String::from_utf8(writer.bytes.lock().expect("trace lock").clone()).expect("UTF-8 logs");
    assert!(text.contains("ignored_informational"));
    assert!(text.contains("rejected_unknown_shape"));
    assert!(text.contains("output_item_type"));
    assert!(text.contains("rejected_output_item"));
    assert!(text.contains("phase=Added"));
    assert!(text.contains("ap-test"));
    assert!(!text.contains("private\nsecret"));
    assert!(text.contains("<invalid-type>"));
    assert_eq!(text.lines().count(), MAX_KINDS + 1);
    assert_eq!(text.matches("unknown_kind_limit").count(), 1);
    assert_eq!(diagnostics.seen.len(), MAX_KINDS);
    assert!(diagnostics.suppressed >= 100);
    assert_eq!(safe_label(Some(&"a".repeat(97))), "<invalid-type>");
    assert_eq!(safe_label(Some("☃")), "<invalid-type>");
}
