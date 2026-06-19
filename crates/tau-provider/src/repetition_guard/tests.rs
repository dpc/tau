use super::*;

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
    assert_eq!(hit.mode, RepetitionMode::Tokens);
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
