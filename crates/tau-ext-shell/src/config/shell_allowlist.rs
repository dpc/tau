//! Bounded core-shell command allowlist parsing and matching.

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::path::Path;

use globset::GlobBuilder;
use regex::bytes::{Regex as BytesRegex, RegexBuilder as BytesRegexBuilder};
use regex::{Regex, RegexBuilder};
use regex_syntax::ast::parse::Parser;
use regex_syntax::ast::{self, Ast};
use serde::de::Error as _;

/// Maximum number of conjunctive rules accepted in one shell allowlist.
pub(super) const MAX_SHELL_ALLOWLIST_RULES: usize = 32;
/// Maximum UTF-8 byte length of one authored glob or regular expression.
pub(super) const MAX_SHELL_ALLOWLIST_PATTERN_BYTES: usize = 2 * 1024;
/// Maximum compiled NFA size for one glob or regular-expression matcher.
pub(super) const MAX_SHELL_ALLOWLIST_COMPILE_BYTES: usize = 256 * 1024;

/// Preserve the semantic difference between an absent allowlist and every
/// present value, including rejecting explicit null as malformed.
pub(super) fn deserialize_shell_allowlist<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<ShellAllowRule>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    /// Deserializes rules without allocating or compiling beyond the configured
    /// rule-count bound.
    struct AllowlistVisitor;

    impl<'de> serde::de::Visitor<'de> for AllowlistVisitor {
        type Value = Vec<ShellAllowRule>;

        /// Describes the only accepted allowlist representation.
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a sequence of shell allowlist rules")
        }

        /// Validates and compiles each rule while enforcing the configured
        /// bound.
        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            if sequence
                .size_hint()
                .is_some_and(|count| MAX_SHELL_ALLOWLIST_RULES < count)
            {
                return Err(A::Error::custom(format!(
                    "shell allowlist permits at most {MAX_SHELL_ALLOWLIST_RULES} rules"
                )));
            }
            let mut rules = Vec::with_capacity(MAX_SHELL_ALLOWLIST_RULES);
            while let Some(raw_rule) = sequence.next_element::<RawShellAllowRule>()? {
                if MAX_SHELL_ALLOWLIST_RULES <= rules.len() {
                    return Err(A::Error::custom(format!(
                        "shell allowlist permits at most {MAX_SHELL_ALLOWLIST_RULES} rules"
                    )));
                }
                rules.push(ShellAllowRule::try_from(raw_rule).map_err(A::Error::custom)?);
            }
            Ok(rules)
        }
    }

    deserializer.deserialize_seq(AllowlistVisitor).map(Some)
}

/// One conjunctive workdir-and-command allowlist rule.
#[derive(Clone, Debug)]
pub(super) struct ShellAllowRule {
    /// Authored absolute workdir glob retained for denial diagnostics.
    workdir: String,
    /// Compiled workdir matcher with component-aware separators.
    workdir_matcher: BytesRegex,
    /// Compiled raw shell-language command matcher retained with its authored
    /// type for matching and denial diagnostics.
    command_matcher: ShellCommandMatcher,
}

impl ShellAllowRule {
    /// Tests this conjunctive rule against an already UTF-8 canonical cwd.
    pub(super) fn matches(&self, canonical_cwd: &str, command: &str) -> bool {
        self.workdir_matcher.is_match(canonical_cwd.as_bytes())
            && self.command_matcher.is_match(command)
    }

    /// Appends the typed matcher and its paired workdir to a denial message.
    pub(super) fn append_diagnostic(&self, message: &mut String) {
        let command = serde_json::to_string(self.command_matcher.pattern())
            .expect("serializing a string to JSON cannot fail");
        let workdir =
            serde_json::to_string(&self.workdir).expect("serializing a string to JSON cannot fail");
        let _ = write!(
            message,
            "\n- {}: {command}\n  workdir: {workdir}",
            self.command_matcher.field_name()
        );
    }

    /// Renders the typed command and workdir selectors as one compact,
    /// deterministic prompt-list entry that remains literal Handlebars template
    /// content.
    pub(super) fn prompt_selector(&self) -> String {
        let command = prompt_json_string(self.command_matcher.pattern());
        let workdir = prompt_json_string(&self.workdir);
        format!(
            "{}: {command}; workdir: {workdir}",
            self.command_matcher.field_name()
        )
    }
}

/// Serializes one selector as a JSON string while escaping braces so authored
/// glob syntax cannot become a Handlebars expression in the prompt template.
fn prompt_json_string(value: &str) -> String {
    serde_json::to_string(value)
        .expect("serializing a string to JSON cannot fail")
        .replace('{', r"\u007b")
        .replace('}', r"\u007d")
}

/// Strict authored representation of one allowlist rule.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawShellAllowRule {
    /// Absolute workdir glob.
    workdir: String,
    /// Optional raw shell-language command glob.
    command: Option<String>,
    /// Optional raw shell-language command regular expression.
    command_regex: Option<String>,
}

/// One compiled raw shell-language command matcher and its authored pattern.
#[derive(Clone, Debug)]
enum ShellCommandMatcher {
    /// A globset-grammar glob with separators treated as ordinary bytes.
    Glob {
        /// Authored glob retained for denial diagnostics.
        pattern: String,
        /// Bounded compiled byte matcher.
        matcher: BytesRegex,
    },
    /// A whole-string Rust regular expression.
    Regex {
        /// Authored regular expression retained for denial diagnostics.
        pattern: String,
        /// Bounded compiled Unicode matcher.
        matcher: Regex,
    },
}

impl ShellCommandMatcher {
    /// Reports whether this matcher accepts the submitted raw command.
    fn is_match(&self, command: &str) -> bool {
        match self {
            Self::Glob { matcher, .. } => matcher.is_match(command.as_bytes()),
            Self::Regex { matcher, .. } => matcher.is_match(command),
        }
    }

    /// Returns the stable configuration field name for denial diagnostics.
    fn field_name(&self) -> &'static str {
        match self {
            Self::Glob { .. } => "command_glob",
            Self::Regex { .. } => "command_regex",
        }
    }

    /// Returns the authored pattern for denial diagnostics.
    fn pattern(&self) -> &str {
        match self {
            Self::Glob { pattern, .. } | Self::Regex { pattern, .. } => pattern,
        }
    }
}

impl TryFrom<RawShellAllowRule> for ShellAllowRule {
    type Error = String;

    /// Validates one authored rule and compiles bounded matchers.
    fn try_from(raw: RawShellAllowRule) -> Result<Self, Self::Error> {
        if !Path::new(&raw.workdir).is_absolute() {
            return Err("shell allowlist workdir glob must be absolute".to_owned());
        }
        require_pattern_limit("workdir", &raw.workdir)?;
        let workdir_matcher = compile_workdir_glob(&raw.workdir)?;
        let command_matcher = match (raw.command, raw.command_regex) {
            (Some(command), None) => {
                require_pattern_limit("command", &command)?;
                ShellCommandMatcher::Glob {
                    matcher: compile_command_glob(&command)?,
                    pattern: command,
                }
            }
            (None, Some(command_regex)) => {
                require_pattern_limit("command_regex", &command_regex)?;
                ShellCommandMatcher::Regex {
                    matcher: compile_command_regex(&command_regex)?,
                    pattern: command_regex,
                }
            }
            (None, None) | (Some(_), Some(_)) => {
                return Err(
                    "shell allowlist rule requires exactly one of `command` or `command_regex`"
                        .to_owned(),
                );
            }
        };
        Ok(Self {
            workdir: raw.workdir,
            workdir_matcher,
            command_matcher,
        })
    }
}

/// Rejects oversized authored patterns before parsers or compilers process
/// them.
fn require_pattern_limit(field: &str, pattern: &str) -> Result<(), String> {
    if MAX_SHELL_ALLOWLIST_PATTERN_BYTES < pattern.len() {
        return Err(format!(
            "shell allowlist `{field}` must not exceed {MAX_SHELL_ALLOWLIST_PATTERN_BYTES} authored UTF-8 bytes"
        ));
    }
    Ok(())
}

/// Compiles one component-aware workdir glob with the shared matcher bound.
fn compile_workdir_glob(pattern: &str) -> Result<BytesRegex, String> {
    compile_glob(pattern, GlobMatcherRole::Workdir)
}

/// Compiles one raw-command glob with the shared matcher bound.
fn compile_command_glob(pattern: &str) -> Result<BytesRegex, String> {
    compile_glob(pattern, GlobMatcherRole::Command)
}

/// Selects the fixed glob grammar configuration and stable diagnostic field.
enum GlobMatcherRole {
    /// Component-aware canonical workdir matching.
    Workdir,
    /// Raw command matching where separators are ordinary characters.
    Command,
}

impl GlobMatcherRole {
    /// Returns the stable diagnostic field name for this role.
    fn field_name(&self) -> &'static str {
        match self {
            Self::Workdir => "workdir",
            Self::Command => "command",
        }
    }

    /// Reports whether `*` must stop at path separators for this role.
    fn literal_separator(&self) -> bool {
        match self {
            Self::Workdir => true,
            Self::Command => false,
        }
    }
}

/// Compiles one globset-grammar glob with its role-specific fixed grammar.
fn compile_glob(pattern: &str, role: GlobMatcherRole) -> Result<BytesRegex, String> {
    let glob = GlobBuilder::new(pattern)
        .literal_separator(role.literal_separator())
        .backslash_escape(true)
        .build()
        .map_err(|_| format!("invalid shell allowlist {} glob", role.field_name()))?;
    BytesRegexBuilder::new(glob.regex())
        .dot_matches_new_line(true)
        .size_limit(MAX_SHELL_ALLOWLIST_COMPILE_BYTES)
        .build()
        .map_err(|error| regex_compile_error(role.field_name(), "glob", error))
}

/// Compiles an implicitly absolute whole-string command regular expression.
fn compile_command_regex(pattern: &str) -> Result<Regex, String> {
    let ast = Parser::new()
        .parse(pattern)
        .map_err(|_| "invalid shell allowlist command regex".to_owned())?;
    if ast_enables_case_insensitive_matching(&ast) {
        return Err("shell allowlist command regex must remain case-sensitive".to_owned());
    }
    let wrapped = format!(r"\A(?:{pattern})\z");
    RegexBuilder::new(&wrapped)
        .size_limit(MAX_SHELL_ALLOWLIST_COMPILE_BYTES)
        .build()
        .map_err(|error| regex_compile_error("command", "regex", error))
}

/// Produces a stable configuration diagnostic without repeating authored
/// patterns.
fn regex_compile_error(field: &str, matcher_type: &str, error: regex::Error) -> String {
    match error {
        regex::Error::CompiledTooBig(_) => format!(
            "shell allowlist {field} {matcher_type} compilation must not exceed {MAX_SHELL_ALLOWLIST_COMPILE_BYTES} bytes"
        ),
        _ => format!("invalid shell allowlist {field} {matcher_type}"),
    }
}

/// Reports whether a parsed expression semantically enables case-insensitive
/// matching.
fn ast_enables_case_insensitive_matching(ast: &Ast) -> bool {
    let mut pending = vec![ast];
    while let Some(ast) = pending.pop() {
        match ast {
            Ast::Flags(flags)
                if flags.flags.flag_state(ast::Flag::CaseInsensitive) == Some(true) =>
            {
                return true;
            }
            Ast::Group(group) => {
                if group
                    .flags()
                    .is_some_and(|flags| flags.flag_state(ast::Flag::CaseInsensitive) == Some(true))
                {
                    return true;
                }
                pending.push(&group.ast);
            }
            Ast::Repetition(repetition) => pending.push(&repetition.ast),
            Ast::Alternation(alternation) => pending.extend(&alternation.asts),
            Ast::Concat(concat) => pending.extend(&concat.asts),
            Ast::Empty(_)
            | Ast::Literal(_)
            | Ast::Dot(_)
            | Ast::Assertion(_)
            | Ast::ClassUnicode(_)
            | Ast::ClassPerl(_)
            | Ast::ClassBracketed(_)
            | Ast::Flags(_) => {}
        }
    }
    false
}
