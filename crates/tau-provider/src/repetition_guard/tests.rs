use super::*;

const GENEROUS_MAX_TAIL_CAPACITY: usize = DEFAULT_TAIL_CHARS * 4 * 2;

fn non_repeating_ascii(min_bytes: usize) -> String {
    let mut text = String::with_capacity(min_bytes);
    let mut index = 0;
    while text.len() < min_bytes {
        text.push_str(&format!("item-{index:08x};"));
        index += 1;
    }
    text
}

fn scalar_suffix(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars().rev().take(max_chars).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

fn detector<'a>(
    guard: &'a StreamRepetitionGuard,
    key: &StreamRepetitionKey,
) -> &'a ExactTailDetector {
    guard
        .detectors
        .get(key)
        .expect("the test component should be admitted")
}

/// Ensures single-character loops are caught only after a long exact
/// suffix.
#[test]
fn detects_long_repeated_character_suffix() {
    let mut guard = StreamRepetitionGuard::new();
    assert!(
        guard
            .push_delta(
                StreamRepetitionKey::AssistantText { output_index: 0 },
                &".".repeat(1023)
            )
            .is_none()
    );
    assert_eq!(
        guard
            .push_delta(StreamRepetitionKey::AssistantText { output_index: 0 }, ".")
            .expect("1024 repeated characters should trigger")
            .mode,
        RepetitionMode::Fragment
    );
}

/// Ensures token-like loops require many exact repeated tokens.
#[test]
fn detects_repeated_token_suffix() {
    let mut guard = StreamRepetitionGuard::new();
    let delta = (0..40)
        .map(|_| "alpha beta gamma")
        .collect::<Vec<_>>()
        .join(" ");
    let hit = guard
        .push_delta(
            StreamRepetitionKey::ReasoningText { output_index: 0 },
            &delta,
        )
        .expect("exact token loop should trigger");
    assert_eq!(
        hit,
        StreamRepetition {
            key: StreamRepetitionKey::ReasoningText { output_index: 0 },
            mode: RepetitionMode::Tokens,
            snippet: "alpha beta gamma".to_owned(),
        }
    );
}

/// Ensures exact repeated line blocks are caught after substantial output.
#[test]
fn detects_repeated_line_block_suffix() {
    let mut guard = StreamRepetitionGuard::new();
    let block = concat!(
        "line one with enough text to matter and a deliberately long payload that exceeds the fragment detector maximum period so only exact line-block matching can catch it\n",
        "line two with enough text to matter and a deliberately long payload that exceeds the fragment detector maximum period so only exact line-block matching can catch it\n",
    );
    let delta = block.repeat(16);
    let hit = guard
        .push_delta(
            StreamRepetitionKey::AssistantText { output_index: 1 },
            &delta,
        )
        .expect("exact line loop should trigger");
    assert_eq!(hit.mode, RepetitionMode::Lines);
}

/// Ensures ordinary prose below conservative thresholds is ignored.
#[test]
fn ignores_non_repeating_prose() {
    let mut guard = StreamRepetitionGuard::new();
    assert!(
        guard
            .push_delta(
                StreamRepetitionKey::AssistantText { output_index: 0 },
                "This is a normal answer with several different words and no tight exact suffix."
            )
            .is_none()
    );
}

/// Ensures a short repeated word sequence never trips the stream guard.
#[test]
fn ignores_short_repeated_words() {
    let mut guard = StreamRepetitionGuard::new();
    let delta = "yes yes yes yes yes yes yes yes";
    assert!(
        guard
            .push_delta(
                StreamRepetitionKey::AssistantText { output_index: 0 },
                delta
            )
            .is_none()
    );
}

/// Ensures repeated code/list prefixes with differing payloads are not
/// fuzzy-matched.
#[test]
fn ignores_repeated_prefixes_with_different_payloads() {
    let mut guard = StreamRepetitionGuard::new();
    let delta = (0..80)
        .map(|index| format!("let item_{index} = compute({index});"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        guard
            .push_delta(
                StreamRepetitionKey::AssistantText { output_index: 0 },
                &delta
            )
            .is_none()
    );
}

/// Ensures exact repeated line blocks below the high threshold do not
/// trigger.
#[test]
fn ignores_repeated_line_block_below_threshold() {
    let mut guard = StreamRepetitionGuard::new();
    let block = "same line one\nsame line two\n";
    assert!(
        guard
            .push_delta(
                StreamRepetitionKey::AssistantText { output_index: 0 },
                &block.repeat(7)
            )
            .is_none()
    );
}

/// Ensures tool-call arguments do not use line-block detection on generated
/// payloads.
#[test]
fn disables_line_block_detection_for_tool_arguments() {
    let mut guard = StreamRepetitionGuard::new();
    let block = concat!(
        "line one with enough text to matter and a deliberately long payload that exceeds the fragment detector maximum period so only exact line-block matching can catch it\n",
        "line two with enough text to matter and a deliberately long payload that exceeds the fragment detector maximum period so only exact line-block matching can catch it\n",
    );
    assert!(
        guard
            .push_delta(
                StreamRepetitionKey::FunctionCallArguments { output_index: 0 },
                &block.repeat(16)
            )
            .is_none()
    );
}

/// Ensures independent stream keys do not combine into a global tail.
#[test]
fn keeps_stream_keys_independent() {
    let mut guard = StreamRepetitionGuard::new();
    for index in 0..4 {
        assert!(
            guard
                .push_delta(
                    StreamRepetitionKey::AssistantText {
                        output_index: index
                    },
                    &".".repeat(200)
                )
                .is_none()
        );
    }
}

/// Ensures final snapshots replace only their component's prior deltas, while
/// preserving detection for both an independent component and a repeating
/// snapshot.
#[test]
fn replace_tail_discards_prior_delta_state() {
    let mut guard = StreamRepetitionGuard::new();
    let primary = StreamRepetitionKey::AssistantText { output_index: 0 };
    let independent = StreamRepetitionKey::ReasoningText { output_index: 1 };
    assert!(
        guard
            .push_delta(primary.clone(), &".".repeat(512))
            .is_none()
    );
    assert!(
        guard
            .push_delta(independent.clone(), &".".repeat(1023))
            .is_none()
    );

    assert!(
        guard
            .replace_tail(primary.clone(), &".".repeat(512))
            .is_none(),
        "a non-repeating final snapshot must not append to prior deltas"
    );
    assert_eq!(
        guard
            .push_delta(independent.clone(), ".")
            .expect("replacing another component must preserve its state")
            .key,
        independent
    );

    let repeated_snapshot = (0..40)
        .map(|_| "alpha beta gamma")
        .collect::<Vec<_>>()
        .join(" ");
    let hit = guard
        .replace_tail(primary.clone(), &repeated_snapshot)
        .expect("a repeating final snapshot should trigger");
    assert_eq!(hit.key, primary);
    assert_eq!(hit.mode, RepetitionMode::Tokens);
}

/// Ensures the default 16-component bound rejects new provider-controlled
/// keys without preventing accepted components from updating and detecting.
#[test]
fn bounds_tracked_stream_components() {
    let mut guard = StreamRepetitionGuard::new();
    for output_index in 0..16 {
        assert!(
            guard
                .push_delta(
                    StreamRepetitionKey::AssistantText { output_index },
                    &format!("non-repeating seed {output_index}")
                )
                .is_none()
        );
    }

    assert!(
        guard
            .push_delta(
                StreamRepetitionKey::AssistantText { output_index: 16 },
                &".".repeat(1024)
            )
            .is_none(),
        "the seventeenth component must not be admitted"
    );
    let hit = guard
        .push_delta(
            StreamRepetitionKey::AssistantText { output_index: 0 },
            &".".repeat(1024),
        )
        .expect("an admitted component must continue to update at the key bound");
    assert_eq!(
        hit.key,
        StreamRepetitionKey::AssistantText { output_index: 0 }
    );
    assert_eq!(hit.mode, RepetitionMode::Fragment);
}

/// Ensures large deltas and replacement snapshots copy only the bounded suffix
/// and never retain input-sized allocation afterward.
#[test]
fn bounds_tail_allocation_for_large_deltas_and_replacements() {
    let mut guard = StreamRepetitionGuard::new();
    let key = StreamRepetitionKey::AssistantText { output_index: 0 };
    let large_delta = non_repeating_ascii(512 * 1024);

    assert!(guard.push_delta(key.clone(), &large_delta).is_none());
    assert_eq!(
        detector(&guard, &key).tail,
        scalar_suffix(&large_delta, DEFAULT_TAIL_CHARS)
    );
    assert!(
        detector(&guard, &key).tail.capacity() <= GENEROUS_MAX_TAIL_CAPACITY,
        "tail capacity should follow the UTF-8 tail bound, not the input size"
    );

    let larger_replacement = non_repeating_ascii(2 * 1024 * 1024);
    assert!(
        guard
            .replace_tail(key.clone(), &larger_replacement)
            .is_none()
    );
    assert_eq!(
        detector(&guard, &key).tail,
        scalar_suffix(&larger_replacement, DEFAULT_TAIL_CHARS)
    );
    assert!(
        detector(&guard, &key).tail.capacity() <= GENEROUS_MAX_TAIL_CAPACITY,
        "a larger replacement should not increase retained capacity"
    );

    assert!(guard.push_delta(key.clone(), "later-small-delta").is_none());
    let combined = format!(
        "{}later-small-delta",
        scalar_suffix(&larger_replacement, DEFAULT_TAIL_CHARS)
    );
    assert_eq!(
        detector(&guard, &key).tail,
        scalar_suffix(&combined, DEFAULT_TAIL_CHARS)
    );

    assert!(
        guard
            .replace_tail(key.clone(), "smaller non-prefix replacement")
            .is_none()
    );
    assert_eq!(
        detector(&guard, &key).tail,
        "smaller non-prefix replacement"
    );
    assert!(
        detector(&guard, &key).tail.capacity() <= GENEROUS_MAX_TAIL_CAPACITY,
        "later small replacements should preserve the bounded allocation"
    );

    assert!(guard.replace_tail(key.clone(), "").is_none());
    assert!(detector(&guard, &key).tail.is_empty());
    assert!(
        detector(&guard, &key).tail.capacity() <= GENEROUS_MAX_TAIL_CAPACITY,
        "empty replacement should not leave input-sized capacity"
    );
}

/// Ensures suffix truncation follows Unicode scalar boundaries for mixed-width
/// text, including boundaries that retain only part of the old tail.
#[test]
fn retains_exact_mixed_width_scalar_suffix() {
    let scalars = ['a', 'é', '中', '🦀'];
    let text = scalars.into_iter().cycle().take(40).collect::<String>();

    for max_chars in [0, 1, 3, 4, 5, 39, 40, 41] {
        let mut detector = ExactTailDetector {
            tail: String::new(),
        };
        detector.push(&text, max_chars);
        assert_eq!(detector.tail, scalar_suffix(&text, max_chars));
    }

    let mut detector = ExactTailDetector {
        tail: "old-aé中🦀".to_owned(),
    };
    let delta = "new-é中🦀";
    let combined = format!("{}{}", detector.tail, delta);
    detector.push(delta, 9);
    assert_eq!(detector.tail, scalar_suffix(&combined, 9));
}
