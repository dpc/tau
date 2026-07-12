//! Shared schemas and parsing helpers for extension-provided UI actions.
//!
//! Extensions publish an [`ActionSchema`] to describe slash-style commands that
//! a UI may invoke. The parser in this crate intentionally follows Tau's simple
//! whitespace-token convention: no shell quoting, no escaping, and at most one
//! rest-string argument at the end of a leaf command.
//!
//! See `ARCH-tau-actions` for the crate boundary and validation contract.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// Current action schema version understood by this crate.
pub const ACTION_SCHEMA_VERSION: u32 = 1;

/// Maximum UTF-8 byte length accepted for command tokens, argument names,
/// action identifiers, and static choice values in extension-published schemas.
pub const MAX_ACTION_TOKEN_BYTES: usize = 128;

/// Maximum UTF-8 byte length accepted for human-readable schema descriptions.
pub const MAX_ACTION_DESCRIPTION_BYTES: usize = 1024;

/// Maximum number of command nodes accepted in one published schema.
pub const MAX_ACTION_COMMANDS: usize = 128;

/// Maximum number of positional arguments accepted on one executable action.
pub const MAX_ACTION_ARGS: usize = 16;

/// Maximum number of static choices accepted on one enum or suggestion list.
pub const MAX_ACTION_CHOICES: usize = 128;

/// Maximum total number of positional arguments accepted in one schema.
pub const MAX_ACTION_TOTAL_ARGS: usize = 256;

/// Maximum total number of static choices accepted in one schema.
pub const MAX_ACTION_TOTAL_CHOICES: usize = 1024;

/// Maximum aggregate UTF-8 bytes accepted across schema-controlled text fields.
pub const MAX_ACTION_TOTAL_TEXT_BYTES: usize = 64 * 1024;

/// Complete action tree published by one extension instance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActionSchema {
    /// Schema version. Version `1` is the initial Tau action schema.
    pub version: u32,
    /// Root slash commands, such as `/email`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<ActionCommand>,
}

impl Default for ActionSchema {
    fn default() -> Self {
        Self {
            version: ACTION_SCHEMA_VERSION,
            roots: Vec::new(),
        }
    }
}

impl ActionSchema {
    /// Validate this schema and return all executable action ids in stable
    /// schema order.
    pub fn executable_action_ids(&self) -> Result<Vec<String>, ValidationError> {
        let mut validator = SchemaValidator::default();
        validator.validate_schema(self)?;
        Ok(validator.action_ids)
    }

    /// Validate this schema.
    ///
    /// Validation enforces the v1 token grammar, leaf/namespace invariants,
    /// duplicate detection, per-field byte limits, and aggregate resource
    /// budgets for command nodes, arguments, choices, and schema-controlled
    /// text.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.executable_action_ids().map(|_| ())
    }

    /// Parse one whitespace-tokenized slash line against this schema.
    pub fn parse_line(&self, line: &str) -> Result<ParsedAction, ParseError> {
        parse_line(self, line)
    }
}

/// One command node in an [`ActionSchema`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActionCommand {
    /// Root name including `/` (for example `/email`) or child name without
    /// `/`.
    ///
    /// Root names must be `/` followed by an ASCII alphanumeric token start and
    /// then only ASCII alphanumeric, `_`, or `-` characters. Child names use
    /// the same token grammar without the leading `/`. Names are also
    /// byte-limited by [`MAX_ACTION_TOKEN_BYTES`].
    pub name: String,
    /// Human-readable command description shown in completions/help.
    pub description: String,
    /// Stable executable action id. Required on leaves, forbidden on
    /// namespaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    /// Positional arguments accepted by an executable leaf.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<ActionArg>,
    /// Child commands. A command with children is a namespace, not executable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ActionCommand>,
}

/// One positional argument accepted by an executable action command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActionArg {
    /// Stable argument name used in parsed named-argument maps.
    ///
    /// Argument names use the same command-token grammar as child command
    /// names: ASCII alphanumeric first character followed by ASCII
    /// alphanumeric, `_`, or `-`, with no slash or whitespace. Names are
    /// byte-limited by [`MAX_ACTION_TOKEN_BYTES`].
    pub name: String,
    /// Human-readable argument description shown in completions/help.
    pub description: String,
    /// Whether the argument must be present.
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    /// Static completion suggestions for open-ended arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<ActionChoice>,
    /// Argument value kind.
    pub kind: ActionArgKind,
}

/// Kind of value accepted by an [`ActionArg`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionArgKind {
    /// A single whitespace-delimited token.
    String,
    /// A signed integer token.
    Integer,
    /// A single token chosen from a closed set.
    Enum {
        /// Allowed values.
        values: Vec<ActionChoice>,
    },
    /// The rest of the line joined back together with single spaces.
    RestString,
}

/// One enum-like choice for an [`ActionArgKind::Enum`] argument.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActionChoice {
    /// Wire value accepted by the parser.
    ///
    /// Choice values are non-empty, contain no whitespace, and are byte-limited
    /// by [`MAX_ACTION_TOKEN_BYTES`].
    pub value: String,
    /// Human-readable choice description.
    pub description: String,
}

/// Parsed action invocation returned by [`parse_line`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedAction {
    /// Stable action id selected by the command path.
    pub action_id: String,
    /// Root command token that matched, including `/`.
    pub root: String,
    /// Full command path tokens, including the root.
    pub command_path: Vec<String>,
    /// Positional argument values in command-schema order.
    pub argv: Vec<String>,
    /// Named argument values keyed by [`ActionArg::name`].
    pub named_args: BTreeMap<String, ParsedArgValue>,
}

/// Typed value parsed for an [`ActionArg`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParsedArgValue {
    /// String-like argument value, including enum and rest-string values.
    String(String),
    /// Integer argument value.
    Integer(i64),
}

/// Error returned when an [`ActionSchema`] is malformed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationError {
    message: String,
}

impl ValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Human-readable validation failure.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ValidationError {}

/// Class of parse failure returned by [`parse_line`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseErrorKind {
    /// The line did not start with a root from the schema.
    UnknownRoot,
    /// The line matched a root but did not select an executable command.
    IncompleteCommand,
    /// The line selected an executable command but supplied bad arguments.
    InvalidArguments,
}

/// Error returned when a slash line cannot be parsed as an action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseError {
    kind: ParseErrorKind,
    message: String,
    usage: Option<String>,
}

impl ParseError {
    fn new(kind: ParseErrorKind, message: impl Into<String>, usage: Option<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            usage,
        }
    }

    /// The parse failure class.
    #[must_use]
    pub fn kind(&self) -> &ParseErrorKind {
        &self.kind
    }

    /// True when the line's slash root is not present in the schema.
    #[must_use]
    pub fn is_unknown_root(&self) -> bool {
        matches!(self.kind, ParseErrorKind::UnknownRoot)
    }

    /// Human-readable parse failure.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Usage string for the matched command path, if one is known.
    #[must_use]
    pub fn usage(&self) -> Option<&str> {
        self.usage.as_deref()
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.usage.as_deref() {
            Some(usage) => write!(f, "{}\nusage: {usage}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Default)]
struct SchemaValidator {
    root_names: BTreeSet<String>,
    action_id_set: BTreeSet<String>,
    action_ids: Vec<String>,
    command_count: usize,
    arg_count: usize,
    choice_count: usize,
    text_bytes: usize,
}

impl SchemaValidator {
    fn validate_schema(&mut self, schema: &ActionSchema) -> Result<(), ValidationError> {
        if schema.version != ACTION_SCHEMA_VERSION {
            return Err(ValidationError::new(format!(
                "unsupported action schema version {}; expected {ACTION_SCHEMA_VERSION}",
                schema.version
            )));
        }
        if schema.roots.len() > MAX_ACTION_COMMANDS {
            return Err(ValidationError::new(format!(
                "action schema declares {} root actions; maximum is {MAX_ACTION_COMMANDS}",
                schema.roots.len()
            )));
        }
        for root in &schema.roots {
            validate_token_len(&root.name, "root action name")?;
            if !is_valid_root_name(&root.name) {
                return Err(ValidationError::new(format!(
                    "invalid root action name `{}`",
                    root.name
                )));
            }
            if !self.root_names.insert(root.name.clone()) {
                return Err(ValidationError::new(format!(
                    "duplicate root action name `{}`",
                    root.name
                )));
            }
            self.validate_command(root, true, &mut vec![root.name.clone()])?;
        }
        Ok(())
    }

    fn validate_command(
        &mut self,
        command: &ActionCommand,
        is_root: bool,
        path: &mut Vec<String>,
    ) -> Result<(), ValidationError> {
        self.command_count += 1;
        if self.command_count > MAX_ACTION_COMMANDS {
            return Err(ValidationError::new(format!(
                "action schema declares more than {MAX_ACTION_COMMANDS} command nodes"
            )));
        }
        self.add_text_bytes(command.name.len(), "command names")?;
        self.add_text_bytes(command.description.len(), "command descriptions")?;
        validate_description_len(&command.description, &path.join(" "))?;
        if !is_root && !is_valid_child_name(&command.name) {
            return Err(ValidationError::new(format!(
                "invalid child action name `{}` in {}",
                command.name,
                path.join(" ")
            )));
        }
        if command.children.is_empty() {
            let Some(action_id) = command.action_id.as_deref() else {
                return Err(ValidationError::new(format!(
                    "action leaf `{}` is missing action_id",
                    path.join(" ")
                )));
            };
            validate_token_len(action_id, "action_id")?;
            self.add_text_bytes(action_id.len(), "action ids")?;
            if action_id.trim().is_empty() || has_whitespace(action_id) {
                return Err(ValidationError::new(format!(
                    "invalid action_id `{action_id}` in {}",
                    path.join(" ")
                )));
            }
            if !self.action_id_set.insert(action_id.to_owned()) {
                return Err(ValidationError::new(format!(
                    "duplicate action_id `{action_id}`"
                )));
            }
            self.action_ids.push(action_id.to_owned());
            self.validate_args(&command.args, &path.join(" "))?;
            return Ok(());
        }

        if command.action_id.is_some() {
            return Err(ValidationError::new(format!(
                "namespace action `{}` must not set action_id",
                path.join(" ")
            )));
        }
        if !command.args.is_empty() {
            return Err(ValidationError::new(format!(
                "namespace action `{}` must not declare args",
                path.join(" ")
            )));
        }
        let mut child_names = BTreeSet::new();
        for child in &command.children {
            validate_token_len(&child.name, "child action name")?;
            if !child_names.insert(child.name.clone()) {
                return Err(ValidationError::new(format!(
                    "duplicate child action name `{}` in {}",
                    child.name,
                    path.join(" ")
                )));
            }
            path.push(child.name.clone());
            self.validate_command(child, false, path)?;
            path.pop();
        }
        Ok(())
    }

    fn validate_args(&mut self, args: &[ActionArg], path: &str) -> Result<(), ValidationError> {
        if args.len() > MAX_ACTION_ARGS {
            return Err(ValidationError::new(format!(
                "action `{path}` declares {} arguments; maximum is {MAX_ACTION_ARGS}",
                args.len()
            )));
        }
        self.arg_count += args.len();
        if self.arg_count > MAX_ACTION_TOTAL_ARGS {
            return Err(ValidationError::new(format!(
                "action schema declares more than {MAX_ACTION_TOTAL_ARGS} total arguments"
            )));
        }
        let mut names = BTreeSet::new();
        let mut seen_optional = false;
        for (index, arg) in args.iter().enumerate() {
            validate_token_len(&arg.name, "argument name")?;
            self.add_text_bytes(arg.name.len(), "argument names")?;
            validate_description_len(
                &arg.description,
                &format!("argument `{}` in {path}", arg.name),
            )?;
            self.add_text_bytes(arg.description.len(), "argument descriptions")?;
            if !is_valid_child_name(&arg.name) {
                return Err(ValidationError::new(format!(
                    "invalid argument name `{}` in {path}",
                    arg.name
                )));
            }
            if !names.insert(arg.name.clone()) {
                return Err(ValidationError::new(format!(
                    "duplicate argument name `{}` in {path}",
                    arg.name
                )));
            }
            if !arg.required {
                seen_optional = true;
            } else if seen_optional {
                return Err(ValidationError::new(format!(
                    "required argument `{}` follows an optional argument in {path}",
                    arg.name
                )));
            }
            if !arg.suggestions.is_empty() {
                self.validate_choices(&arg.suggestions, &arg.name, path)?;
            }
            match &arg.kind {
                ActionArgKind::RestString if index + 1 != args.len() => {
                    return Err(ValidationError::new(format!(
                        "rest argument `{}` must be last in {path}",
                        arg.name
                    )));
                }
                ActionArgKind::Enum { values } => self.validate_choices(values, &arg.name, path)?,
                ActionArgKind::String | ActionArgKind::Integer | ActionArgKind::RestString => {}
            }
        }
        Ok(())
    }

    fn validate_choices(
        &mut self,
        values: &[ActionChoice],
        arg_name: &str,
        path: &str,
    ) -> Result<(), ValidationError> {
        if values.is_empty() {
            return Err(ValidationError::new(format!(
                "enum argument `{arg_name}` in {path} must declare at least one value"
            )));
        }
        if values.len() > MAX_ACTION_CHOICES {
            return Err(ValidationError::new(format!(
                "argument `{arg_name}` in {path} declares {} choices; maximum is {MAX_ACTION_CHOICES}",
                values.len()
            )));
        }
        self.choice_count += values.len();
        if self.choice_count > MAX_ACTION_TOTAL_CHOICES {
            return Err(ValidationError::new(format!(
                "action schema declares more than {MAX_ACTION_TOTAL_CHOICES} total choices"
            )));
        }
        let mut seen = BTreeSet::new();
        for choice in values {
            validate_token_len(&choice.value, "choice value")?;
            self.add_text_bytes(choice.value.len(), "choice values")?;
            validate_description_len(
                &choice.description,
                &format!("choice `{}` for `{arg_name}` in {path}", choice.value),
            )?;
            self.add_text_bytes(choice.description.len(), "choice descriptions")?;
            if choice.value.is_empty() || has_whitespace(&choice.value) {
                return Err(ValidationError::new(format!(
                    "invalid enum value `{}` for `{arg_name}` in {path}",
                    choice.value
                )));
            }
            if !seen.insert(choice.value.clone()) {
                return Err(ValidationError::new(format!(
                    "duplicate enum value `{}` for `{arg_name}` in {path}",
                    choice.value
                )));
            }
        }
        Ok(())
    }

    fn add_text_bytes(&mut self, bytes: usize, field: &str) -> Result<(), ValidationError> {
        self.text_bytes = self.text_bytes.saturating_add(bytes);
        if self.text_bytes > MAX_ACTION_TOTAL_TEXT_BYTES {
            return Err(ValidationError::new(format!(
                "action schema text exceeds {MAX_ACTION_TOTAL_TEXT_BYTES} total bytes while reading {field}"
            )));
        }
        Ok(())
    }
}

fn validate_token_len(value: &str, field: &str) -> Result<(), ValidationError> {
    if value.len() > MAX_ACTION_TOKEN_BYTES {
        return Err(ValidationError::new(format!(
            "{field} is {} bytes; maximum is {MAX_ACTION_TOKEN_BYTES}",
            value.len()
        )));
    }
    Ok(())
}

fn validate_description_len(value: &str, context: &str) -> Result<(), ValidationError> {
    if value.len() > MAX_ACTION_DESCRIPTION_BYTES {
        return Err(ValidationError::new(format!(
            "description for {context} is {} bytes; maximum is {MAX_ACTION_DESCRIPTION_BYTES}",
            value.len()
        )));
    }
    Ok(())
}

/// Parse one whitespace-tokenized slash line against a schema.
pub fn parse_line(schema: &ActionSchema, line: &str) -> Result<ParsedAction, ParseError> {
    if let Err(error) = schema.validate() {
        return Err(ParseError::new(
            ParseErrorKind::InvalidArguments,
            format!("invalid action schema: {error}"),
            None,
        ));
    }

    let tokens: Vec<&str> = line.split_whitespace().collect();
    let Some(root_token) = tokens.first().copied() else {
        return Err(ParseError::new(
            ParseErrorKind::UnknownRoot,
            "empty action line",
            None,
        ));
    };
    let Some(mut command) = schema.roots.iter().find(|root| root.name == root_token) else {
        return Err(ParseError::new(
            ParseErrorKind::UnknownRoot,
            format!("unknown action root `{root_token}`"),
            None,
        ));
    };

    let mut index = 1;
    let mut path = vec![command.name.clone()];
    while let Some(next_token) = tokens.get(index).copied() {
        let Some(child) = command
            .children
            .iter()
            .find(|child| child.name == next_token)
        else {
            break;
        };
        command = child;
        path.push(command.name.clone());
        index += 1;
    }

    if !command.children.is_empty() {
        let children = command
            .children
            .iter()
            .map(|child| child.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ParseError::new(
            ParseErrorKind::IncompleteCommand,
            format!("{} requires a subcommand ({children})", path.join(" ")),
            Some(format!(
                "{} <{}>",
                path.join(" "),
                children.replace(", ", "|")
            )),
        ));
    }

    let Some(action_id) = command.action_id.clone() else {
        return Err(ParseError::new(
            ParseErrorKind::IncompleteCommand,
            format!("{} is not executable", path.join(" ")),
            Some(usage_for(command, &path)),
        ));
    };

    let remaining = &tokens[index..];
    let (argv, named_args) = parse_args(command, remaining, &path)?;
    Ok(ParsedAction {
        action_id,
        root: root_token.to_owned(),
        command_path: path,
        argv,
        named_args,
    })
}

fn parse_args(
    command: &ActionCommand,
    tokens: &[&str],
    path: &[String],
) -> Result<(Vec<String>, BTreeMap<String, ParsedArgValue>), ParseError> {
    let mut token_index = 0;
    let mut argv = Vec::new();
    let mut named_args = BTreeMap::new();
    for arg in &command.args {
        match &arg.kind {
            ActionArgKind::RestString => {
                let value = tokens[token_index..].join(" ");
                if value.is_empty() {
                    if arg.required {
                        return Err(missing_arg(command, path, &arg.name));
                    }
                } else {
                    argv.push(value.clone());
                    named_args.insert(arg.name.clone(), ParsedArgValue::String(value));
                }
                token_index = tokens.len();
            }
            ActionArgKind::String => {
                let Some(token) = tokens.get(token_index).copied() else {
                    if arg.required {
                        return Err(missing_arg(command, path, &arg.name));
                    }
                    continue;
                };
                argv.push(token.to_owned());
                named_args.insert(arg.name.clone(), ParsedArgValue::String(token.to_owned()));
                token_index += 1;
            }
            ActionArgKind::Integer => {
                let Some(token) = tokens.get(token_index).copied() else {
                    if arg.required {
                        return Err(missing_arg(command, path, &arg.name));
                    }
                    continue;
                };
                let value = token.parse::<i64>().map_err(|_| {
                    ParseError::new(
                        ParseErrorKind::InvalidArguments,
                        format!("argument `{}` must be an integer", arg.name),
                        Some(usage_for(command, path)),
                    )
                })?;
                argv.push(token.to_owned());
                named_args.insert(arg.name.clone(), ParsedArgValue::Integer(value));
                token_index += 1;
            }
            ActionArgKind::Enum { values } => {
                let Some(token) = tokens.get(token_index).copied() else {
                    if arg.required {
                        return Err(missing_arg(command, path, &arg.name));
                    }
                    continue;
                };
                if !values.iter().any(|choice| choice.value == token) {
                    let expected = values
                        .iter()
                        .map(|choice| choice.value.as_str())
                        .collect::<Vec<_>>()
                        .join("|");
                    return Err(ParseError::new(
                        ParseErrorKind::InvalidArguments,
                        format!("argument `{}` must be one of {expected}", arg.name),
                        Some(usage_for(command, path)),
                    ));
                }
                argv.push(token.to_owned());
                named_args.insert(arg.name.clone(), ParsedArgValue::String(token.to_owned()));
                token_index += 1;
            }
        }
    }

    if token_index < tokens.len() {
        return Err(ParseError::new(
            ParseErrorKind::InvalidArguments,
            format!("too many arguments for {}", path.join(" ")),
            Some(usage_for(command, path)),
        ));
    }

    Ok((argv, named_args))
}

fn missing_arg(command: &ActionCommand, path: &[String], arg_name: &str) -> ParseError {
    ParseError::new(
        ParseErrorKind::InvalidArguments,
        format!("missing required argument `{arg_name}`"),
        Some(usage_for(command, path)),
    )
}

/// Build a simple usage string for a command leaf.
#[must_use]
pub fn usage_for(command: &ActionCommand, path: &[String]) -> String {
    let mut usage = path.join(" ");
    for arg in &command.args {
        usage.push(' ');
        let arg_usage = match &arg.kind {
            ActionArgKind::String => arg.name.clone(),
            ActionArgKind::Integer => format!("{}:int", arg.name),
            ActionArgKind::Enum { values } => values
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>()
                .join("|"),
            ActionArgKind::RestString => format!("{}...", arg.name),
        };
        if arg.required {
            usage.push('<');
            usage.push_str(&arg_usage);
            usage.push('>');
        } else {
            usage.push('[');
            usage.push_str(&arg_usage);
            usage.push(']');
        }
    }
    usage
}

/// Return whether a string is a valid root slash command name.
///
/// Valid roots are `/` followed by an ASCII alphanumeric token start and then
/// only ASCII alphanumeric, `_`, or `-` characters.
#[must_use]
pub fn is_valid_root_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('/') else {
        return false;
    };
    is_valid_command_segment(rest)
}

/// Return whether a string is a valid child command or argument name.
///
/// Valid child and argument names start with an ASCII alphanumeric character
/// and then contain only ASCII alphanumeric, `_`, or `-` characters.
#[must_use]
pub fn is_valid_child_name(name: &str) -> bool {
    is_valid_command_segment(name)
}

fn is_valid_command_segment(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

fn has_whitespace(s: &str) -> bool {
    s.chars().any(char::is_whitespace)
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests;
