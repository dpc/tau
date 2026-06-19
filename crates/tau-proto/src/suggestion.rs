//! Small deterministic suggestion helpers shared by protocol users.

/// Maximum iterator items observed by [`nearest_name_suggestion`] before
/// filtering or scoring.
pub const MAX_SUGGESTION_CANDIDATES: usize = 128;

/// Maximum characters considered from any suggested name.
pub const MAX_SUGGESTION_NAME_CHARS: usize = 80;

/// Returns a deterministic, tie-safe nearest-name suggestion.
///
/// The helper is intentionally conservative: it returns no suggestion for poor
/// edit-distance matches, no suggestion when multiple candidates are equally
/// close, and no suggestion when the candidate set exceeds the bounded work
/// budget. Candidates are sorted before scoring so the outcome is independent
/// of registry or filesystem iteration order.
#[must_use]
pub fn nearest_name_suggestion<'a>(
    requested: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let requested_len = bounded_char_count(requested, MAX_SUGGESTION_NAME_CHARS)?;
    if requested_len == 0 {
        return None;
    }
    let mut bounded_candidates: Vec<&'a str> = Vec::new();
    let mut observed_candidates = 0usize;
    for candidate in candidates {
        observed_candidates += 1;
        if MAX_SUGGESTION_CANDIDATES < observed_candidates {
            return None;
        }
        if bounded_char_count(candidate, MAX_SUGGESTION_NAME_CHARS).is_some() {
            bounded_candidates.push(candidate);
        }
    }
    bounded_candidates.sort_unstable();
    bounded_candidates.dedup();

    let max_distance = std::cmp::max(2, requested_len / 3);
    let mut best: Option<(&str, usize)> = None;
    let mut tied = false;
    for candidate in bounded_candidates {
        let distance = levenshtein_distance(requested, candidate);
        if max_distance < distance {
            continue;
        }
        match best {
            None => {
                best = Some((candidate, distance));
                tied = false;
            }
            Some((_, best_distance)) if distance < best_distance => {
                best = Some((candidate, distance));
                tied = false;
            }
            Some((_, best_distance)) if distance == best_distance => {
                tied = true;
            }
            Some(_) => {}
        }
    }

    if tied {
        None
    } else {
        best.map(|(candidate, _)| candidate.to_owned())
    }
}

fn levenshtein_distance(left: &str, right: &str) -> usize {
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];
    for (left_idx, left_ch) in left.iter().enumerate() {
        current[0] = left_idx + 1;
        for (right_idx, right_ch) in right.iter().enumerate() {
            let substitution = previous[right_idx] + usize::from(left_ch != right_ch);
            let insertion = current[right_idx] + 1;
            let deletion = previous[right_idx + 1] + 1;
            current[right_idx + 1] = substitution.min(insertion).min(deletion);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

fn bounded_char_count(text: &str, max_chars: usize) -> Option<usize> {
    let mut count = 0usize;
    for _ in text.chars() {
        count += 1;
        if max_chars < count {
            return None;
        }
    }
    Some(count)
}
