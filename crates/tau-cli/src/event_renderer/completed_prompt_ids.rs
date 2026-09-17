//! Lossless compact storage for completed provider prompt identifiers.

use std::collections::{HashMap, HashSet};

/// Exact completed-prompt membership with compact canonical numeric suffixes.
#[derive(Default)]
pub(super) struct CompletedPromptIds {
    /// Sorted inclusive numeric ranges keyed by the full prefix before the
    /// suffix.
    numeric_ranges: HashMap<String, Vec<(u64, u64)>>,
    /// Exact storage for identifiers without a canonical numeric suffix.
    fallback: HashSet<tau_proto::AgentPromptId>,
}

impl CompletedPromptIds {
    /// Inserts an identifier and reports whether it was newly present.
    pub(super) fn insert(&mut self, prompt_id: tau_proto::AgentPromptId) -> bool {
        let Some((prefix, suffix)) = canonical_numeric_suffix(prompt_id.as_str()) else {
            return self.fallback.insert(prompt_id);
        };
        if let Some(ranges) = self.numeric_ranges.get_mut(prefix) {
            return insert_number(ranges, suffix);
        }
        self.numeric_ranges
            .insert(prefix.to_owned(), vec![(suffix, suffix)]);
        true
    }

    /// Reports whether the exact identifier is present.
    pub(super) fn contains(&self, prompt_id: &tau_proto::AgentPromptId) -> bool {
        let Some((prefix, suffix)) = canonical_numeric_suffix(prompt_id.as_str()) else {
            return self.fallback.contains(prompt_id);
        };
        self.numeric_ranges
            .get(prefix)
            .is_some_and(|ranges| contains_number(ranges, suffix))
    }

    /// Removes an identifier and reports whether it was present.
    pub(super) fn remove(&mut self, prompt_id: &tau_proto::AgentPromptId) -> bool {
        let Some((prefix, suffix)) = canonical_numeric_suffix(prompt_id.as_str()) else {
            return self.fallback.remove(prompt_id);
        };
        let Some(ranges) = self.numeric_ranges.get_mut(prefix) else {
            return false;
        };
        let removed = remove_number(ranges, suffix);
        if ranges.is_empty() {
            self.numeric_ranges.remove(prefix);
        }
        removed
    }
}

/// Splits an ID into its retained prefix and exactly round-tripping numeric
/// suffix.
fn canonical_numeric_suffix(prompt_id: &str) -> Option<(&str, u64)> {
    let suffix_start = prompt_id.rfind('-')?.checked_add(1)?;
    let (prefix, suffix) = prompt_id.split_at(suffix_start);
    if suffix.len() > 1 && suffix.starts_with('0') {
        return None;
    }
    let number = suffix.parse::<u64>().ok()?;
    Some((prefix, number))
}

/// Reports exact membership in sorted disjoint inclusive ranges.
fn contains_number(ranges: &[(u64, u64)], number: u64) -> bool {
    let index = ranges.partition_point(|(_, end)| *end < number);
    ranges.get(index).is_some_and(|(start, _)| *start <= number)
}

/// Inserts a number into sorted disjoint inclusive ranges.
fn insert_number(ranges: &mut Vec<(u64, u64)>, number: u64) -> bool {
    let index = ranges.partition_point(|(_, end)| *end < number);
    if ranges.get(index).is_some_and(|(start, _)| *start <= number) {
        return false;
    }

    let joins_left = index
        .checked_sub(1)
        .and_then(|left| ranges[left].1.checked_add(1))
        == Some(number);
    let joins_right = number
        .checked_add(1)
        .zip(ranges.get(index).map(|(start, _)| *start))
        .is_some_and(|(next, start)| next == start);

    match (joins_left, joins_right) {
        (true, true) => {
            let right_end = ranges.remove(index).1;
            ranges[index - 1].1 = right_end;
        }
        (true, false) => ranges[index - 1].1 = number,
        (false, true) => ranges[index].0 = number,
        (false, false) => ranges.insert(index, (number, number)),
    }
    true
}

/// Removes a number from sorted disjoint inclusive ranges.
fn remove_number(ranges: &mut Vec<(u64, u64)>, number: u64) -> bool {
    let index = ranges.partition_point(|(_, end)| *end < number);
    let Some(&(start, end)) = ranges.get(index) else {
        return false;
    };
    if number < start {
        return false;
    }

    match (number == start, number == end) {
        (true, true) => {
            ranges.remove(index);
        }
        (true, false) => ranges[index].0 = start + 1,
        (false, true) => ranges[index].1 = end - 1,
        (false, false) => {
            ranges[index].1 = number - 1;
            ranges.insert(index + 1, (number + 1, end));
        }
    }
    true
}

#[cfg(test)]
mod tests;
