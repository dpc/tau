use std::sync::Mutex;

use tau_proto::{CborValue, ToolCallId};

/// Deterministic work observed at production ownership boundaries.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct OwnershipWork {
    /// Small identity clones made after the payload was separated.
    pub(crate) identity_clones: usize,
    /// Deep argument clones made after admission.
    pub(crate) argument_clones: usize,
    /// Approximate argument bytes retained by the scheduler job.
    pub(crate) queued_argument_bytes: usize,
    /// Address of the large text allocation at ingress.
    pub(crate) ingress_text_ptr: usize,
    /// Address of the same allocation when execution reassembles the call.
    pub(crate) execution_text_ptr: usize,
    /// Bytes copied into the independently retained lock-wait snapshot.
    pub(crate) lock_wait_snapshot_bytes: usize,
}

static PROBES: Mutex<Vec<(String, OwnershipWork)>> = Mutex::new(Vec::new());

/// Begin observing one uniquely identified production call.
pub(crate) fn start(call_id: &str) {
    let mut probes = PROBES.lock().expect("ownership probe poisoned");
    probes.retain(|(existing, _)| existing != call_id);
    probes.push((call_id.to_owned(), OwnershipWork::default()));
}

/// Finish observation and return the recorded work.
pub(crate) fn finish(call_id: &str) -> OwnershipWork {
    let mut probes = PROBES.lock().expect("ownership probe poisoned");
    let position = probes
        .iter()
        .position(|(existing, _)| existing == call_id)
        .expect("ownership probe was started");
    probes.swap_remove(position).1
}

pub(super) fn record_split(call_id: &ToolCallId, arguments: &CborValue) {
    update(call_id, |work| {
        work.ingress_text_ptr = large_text_ptr(arguments);
    });
}

pub(super) fn record_reassembly(call_id: &ToolCallId, arguments: &CborValue) {
    update(call_id, |work| {
        work.execution_text_ptr = large_text_ptr(arguments);
        work.argument_clones += usize::from(work.execution_text_ptr != work.ingress_text_ptr);
    });
}

pub(super) fn record_identity_clone(call_id: &ToolCallId) {
    update(call_id, |work| work.identity_clones += 1);
}

/// Record the scheduler's production byte-accounting result.
pub(crate) fn record_queued_bytes(call_id: &ToolCallId, bytes: usize) {
    update(call_id, |work| work.queued_argument_bytes = bytes);
}

/// Record the bounded display bytes copied by the production wait path.
pub(crate) fn record_wait_snapshot(call_id: &ToolCallId, bytes: usize) {
    update(call_id, |work| work.lock_wait_snapshot_bytes = bytes);
}

fn update(call_id: &ToolCallId, update: impl FnOnce(&mut OwnershipWork)) {
    let mut probes = PROBES.lock().expect("ownership probe poisoned");
    if let Some((_, work)) = probes
        .iter_mut()
        .find(|(expected, _)| expected == call_id.as_str())
    {
        update(work);
    }
}

fn large_text_ptr(value: &CborValue) -> usize {
    largest_text(value).map_or(0, |text| text.as_ptr() as usize)
}

fn largest_text(value: &CborValue) -> Option<&str> {
    match value {
        CborValue::Text(text) => Some(text),
        CborValue::Array(values) => values
            .iter()
            .filter_map(largest_text)
            .max_by_key(|text| text.len()),
        CborValue::Map(entries) => entries
            .iter()
            .flat_map(|(key, value)| [largest_text(key), largest_text(value)])
            .flatten()
            .max_by_key(|text| text.len()),
        _ => None,
    }
}
