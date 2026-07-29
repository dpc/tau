//! Conservative exact-match guard for provider streaming loops.
//!
//! Create one [`StreamRepetitionGuard`] for one provider generation and call
//! [`StreamRepetitionGuard::push_delta`] before appending or emitting each
//! assistant-text, reasoning-text, function-argument, or custom-tool-input
//! delta. Each [`StreamRepetitionKey`] has an independent bounded tail; there
//! is no global stream tail and no cross-component matching.
//!
//! The guard intentionally implements only high-confidence exact suffix checks:
//! no entropy, fuzzy matching, semantic similarity, or near-duplicate line
//! detection. The current modes require substantial volume before firing:
//! roughly 1024 repeated characters with at least 8 exact fragment repetitions,
//! 120 whitespace-delimited token-like units with at least 8 exact n-gram
//! repetitions, or 1024 repeated line-block characters with at least 8 exact
//! line-block repetitions. Exact line-block mode is disabled for tool-argument
//! keys, where generated files/fixtures are more likely to contain legitimate
//! repeated line structure; fragment and token checks still cover tight
//! argument loops such as `_clone_clone_clone...`.
//!
//! State is bounded by a small key count, a per-key tail, and bounded snippets
//! in [`StreamRepetition`] diagnostics. Tests for parser integrations should
//! follow `docs/testing.md#provider-stream-repetition-guard`.

use std::collections::BTreeMap;

const DEFAULT_MAX_KEYS: usize = 16;
const DEFAULT_TAIL_CHARS: usize = 4096;
const MAX_FRAGMENT_CHARS: usize = 160;
const MIN_FRAGMENT_REPEATED_CHARS: usize = 1024;
const MIN_FRAGMENT_REPETITIONS: usize = 8;
const MAX_TOKEN_PERIOD: usize = 20;
const MIN_TOKEN_REPETITIONS: usize = 8;
const MIN_REPEATED_TOKENS: usize = 120;
const MAX_LINE_PERIOD: usize = 8;
const MIN_LINE_REPETITIONS: usize = 8;
const MIN_LINE_REPEATED_CHARS: usize = 1024;
const MAX_SNIPPET_CHARS: usize = 160;

/// A bounded exact repetition guard for one provider stream.
#[derive(Debug)]
pub struct StreamRepetitionGuard {
    /// Per-component exact tail detectors.
    detectors: BTreeMap<StreamRepetitionKey, ExactTailDetector>,
    /// Maximum number of components tracked for one provider generation.
    max_keys: usize,
    /// Maximum characters retained per component.
    max_tail_chars: usize,
}

/// Identifies one independently-checked stream component.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum StreamRepetitionKey {
    /// Assistant-visible message text for a provider output item.
    AssistantText { output_index: usize },
    /// Reasoning/thinking text for a provider output item.
    ReasoningText { output_index: usize },
    /// Function-call argument JSON text for a provider output item.
    FunctionCallArguments { output_index: usize },
    /// Custom-tool input text for a provider output item.
    CustomToolInput { output_index: usize },
}

/// Description of a conservative exact repetition detected in a stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamRepetition {
    /// Component whose bounded tail repeated.
    pub key: StreamRepetitionKey,
    /// Exact detection mode that fired.
    pub mode: RepetitionMode,
    /// Human-readable bounded snippet from the repeated suffix.
    pub snippet: String,
}

impl std::fmt::Display for StreamRepetition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "detected repeated provider stream output ({:?} {:?}: {:?})",
            self.key, self.mode, self.snippet
        )
    }
}

/// Exact repetition mode that fired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepetitionMode {
    /// A short exact text fragment repeated many times at the suffix.
    Fragment,
    /// A whitespace-delimited token sequence repeated at the suffix.
    Tokens,
    /// A block of exact lines repeated at the suffix.
    Lines,
}

#[derive(Debug)]
struct ExactTailDetector {
    /// Bounded trailing text for one stream component.
    tail: String,
}

impl Default for StreamRepetitionGuard {
    fn default() -> Self {
        Self {
            detectors: BTreeMap::new(),
            max_keys: DEFAULT_MAX_KEYS,
            max_tail_chars: DEFAULT_TAIL_CHARS,
        }
    }
}

impl StreamRepetitionGuard {
    /// Create a guard with default conservative bounds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one provider delta to the selected stream component.
    pub fn push_delta(
        &mut self,
        key: StreamRepetitionKey,
        delta: &str,
    ) -> Option<StreamRepetition> {
        if delta.is_empty() {
            return None;
        }
        if !self.detectors.contains_key(&key) && self.detectors.len() >= self.max_keys {
            return None;
        }
        let detector = self
            .detectors
            .entry(key.clone())
            .or_insert_with(|| ExactTailDetector {
                tail: String::new(),
            });
        detector.push(delta, self.max_tail_chars);
        detector
            .detect(&key)
            .map(|(mode, snippet)| StreamRepetition { key, mode, snippet })
    }

    /// Replace the tracked tail for a component with a final snapshot.
    ///
    /// Providers should use this for non-prefix `*.done` snapshots so prior
    /// deltas are not double-counted.
    pub fn replace_tail(
        &mut self,
        key: StreamRepetitionKey,
        text: &str,
    ) -> Option<StreamRepetition> {
        if !self.detectors.contains_key(&key) && self.detectors.len() >= self.max_keys {
            return None;
        }
        let detector = self
            .detectors
            .entry(key.clone())
            .or_insert_with(|| ExactTailDetector {
                tail: String::new(),
            });
        detector.tail.clear();
        detector.push(text, self.max_tail_chars);
        detector
            .detect(&key)
            .map(|(mode, snippet)| StreamRepetition { key, mode, snippet })
    }
}

impl ExactTailDetector {
    fn push(&mut self, delta: &str, max_tail_chars: usize) {
        self.tail.push_str(delta);
        let chars = self.tail.chars().count();
        if max_tail_chars < chars {
            let drop = chars - max_tail_chars;
            let byte = self
                .tail
                .char_indices()
                .nth(drop)
                .map(|(index, _)| index)
                .unwrap_or(self.tail.len());
            self.tail.drain(..byte);
        }
    }

    fn detect(&self, key: &StreamRepetitionKey) -> Option<(RepetitionMode, String)> {
        self.detect_fragment()
            .or_else(|| self.detect_tokens())
            .or_else(|| {
                (!key.is_tool_argument())
                    .then(|| self.detect_lines())
                    .flatten()
            })
    }

    fn detect_fragment(&self) -> Option<(RepetitionMode, String)> {
        let chars: Vec<char> = self.tail.chars().collect();
        for period in 1..=MAX_FRAGMENT_CHARS.min(chars.len() / MIN_FRAGMENT_REPETITIONS) {
            let reps = suffix_repetitions(&chars, period);
            if reps < MIN_FRAGMENT_REPETITIONS {
                continue;
            }
            let repeated = reps * period;
            if MIN_FRAGMENT_REPEATED_CHARS <= repeated {
                return Some((
                    RepetitionMode::Fragment,
                    bounded_chars(&chars[chars.len() - period..].iter().collect::<String>()),
                ));
            }
        }
        None
    }

    fn detect_tokens(&self) -> Option<(RepetitionMode, String)> {
        let tokens: Vec<&str> = self.tail.split_whitespace().collect();
        for period in 1..=MAX_TOKEN_PERIOD.min(tokens.len() / MIN_TOKEN_REPETITIONS) {
            let reps = suffix_repetitions(&tokens, period);
            if MIN_TOKEN_REPETITIONS <= reps && reps * period >= MIN_REPEATED_TOKENS {
                return Some((
                    RepetitionMode::Tokens,
                    bounded_chars(&tokens[tokens.len() - period..].join(" ")),
                ));
            }
        }
        None
    }

    fn detect_lines(&self) -> Option<(RepetitionMode, String)> {
        let lines: Vec<&str> = self.tail.lines().collect();
        for period in 1..=MAX_LINE_PERIOD.min(lines.len() / MIN_LINE_REPETITIONS) {
            let reps = suffix_repetitions(&lines, period);
            if reps < MIN_LINE_REPETITIONS {
                continue;
            }
            let repeated = lines[lines.len() - reps * period..]
                .join("\n")
                .chars()
                .count();
            if MIN_LINE_REPEATED_CHARS <= repeated {
                return Some((
                    RepetitionMode::Lines,
                    bounded_chars(&lines[lines.len() - period..].join("\n")),
                ));
            }
        }
        None
    }
}

impl StreamRepetitionKey {
    fn is_tool_argument(&self) -> bool {
        matches!(
            self,
            Self::FunctionCallArguments { .. } | Self::CustomToolInput { .. }
        )
    }
}

fn suffix_repetitions<T: Eq>(items: &[T], period: usize) -> usize {
    let len = items.len();
    if period == 0 || len < period {
        return 0;
    }
    let pattern = &items[len - period..];
    let mut reps = 1;
    while len >= (reps + 1) * period {
        let start = len - (reps + 1) * period;
        let end = len - reps * period;
        if &items[start..end] != pattern {
            break;
        }
        reps += 1;
    }
    reps
}

fn bounded_chars(text: &str) -> String {
    let mut out = text.chars().take(MAX_SNIPPET_CHARS).collect::<String>();
    if text.chars().nth(MAX_SNIPPET_CHARS).is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests;
