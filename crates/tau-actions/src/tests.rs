use super::*;

fn string_arg(name: &str) -> ActionArg {
    ActionArg {
        name: name.to_owned(),
        description: format!("{name} value"),
        required: true,
        suggestions: Vec::new(),
        kind: ActionArgKind::String,
    }
}

fn rest_arg(name: &str) -> ActionArg {
    arg(name, ActionArgKind::RestString, true)
}

fn arg(name: &str, kind: ActionArgKind, required: bool) -> ActionArg {
    ActionArg {
        name: name.to_owned(),
        description: format!("{name} value"),
        required,
        suggestions: Vec::new(),
        kind,
    }
}

fn choice(value: impl Into<String>) -> ActionChoice {
    ActionChoice {
        value: value.into(),
        description: String::new(),
    }
}

fn enum_arg(name: &str, values: Vec<ActionChoice>) -> ActionArg {
    arg(name, ActionArgKind::Enum { values }, true)
}

fn schema_with_leaf(args: Vec<ActionArg>) -> ActionSchema {
    ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![leaf(":test", "test.action", args)],
    }
}

fn leaf(name: &str, action_id: &str, args: Vec<ActionArg>) -> ActionCommand {
    ActionCommand {
        name: name.to_owned(),
        description: format!("{name} action"),
        action_id: Some(action_id.to_owned()),
        args,
        children: Vec::new(),
    }
}

fn group(name: &str, children: Vec<ActionCommand>) -> ActionCommand {
    ActionCommand {
        name: name.to_owned(),
        description: format!("{name} commands"),
        action_id: None,
        args: Vec::new(),
        children,
    }
}

fn email_schema() -> ActionSchema {
    ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![ActionCommand {
            name: ":email".to_owned(),
            description: "Review email approvals".to_owned(),
            action_id: None,
            args: Vec::new(),
            children: vec![
                group(
                    "out",
                    vec![
                        leaf("list", "email.out.list", Vec::new()),
                        leaf("approve", "email.out.approve", vec![string_arg("id")]),
                    ],
                ),
                group(
                    "draft",
                    vec![leaf("note", "email.draft.note", vec![rest_arg("text")])],
                ),
            ],
        }],
    }
}

/// Ensures a representative nested schema remains valid so extensions can
/// publish namespace-style extension actions without parser regressions.
#[test]
fn schema_validation_accepts_nested_executable_leaves() {
    let ids = email_schema()
        .executable_action_ids()
        .expect("schema should validate");

    assert_eq!(
        ids,
        vec![
            "email.out.list".to_owned(),
            "email.out.approve".to_owned(),
            "email.draft.note".to_owned(),
        ]
    );
}

/// Prevents ambiguous action routing by rejecting duplicate executable ids
/// within one extension-published schema.
#[test]
fn schema_validation_rejects_duplicate_action_ids() {
    let schema = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![ActionCommand {
            name: ":email".to_owned(),
            description: String::new(),
            action_id: None,
            args: Vec::new(),
            children: vec![
                leaf("one", "email.same", Vec::new()),
                leaf("two", "email.same", Vec::new()),
            ],
        }],
    };

    let error = schema.validate().expect_err("duplicate id should fail");
    assert!(error.message().contains("duplicate action_id `email.same`"));
}

/// Keeps dynamic action roots in the command namespace by requiring the
/// leading colon form used by the CLI and harness.
#[test]
fn schema_validation_rejects_invalid_root_names() {
    let schema = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![leaf("email", "email.root", Vec::new())],
    };

    let error = schema
        .validate()
        .expect_err("root without colon should fail");
    assert!(error.message().contains("invalid root action name"));
}

/// Prevents child command tokens from smuggling slash paths or other
/// non-command syntax into UI completions and parser diagnostics.
#[test]
fn schema_validation_rejects_invalid_child_names() {
    for name in ["bad/name", "bad.name", "-bad", "åbad", "bad name"] {
        let schema = ActionSchema {
            version: ACTION_SCHEMA_VERSION,
            roots: vec![ActionCommand {
                name: ":email".to_owned(),
                description: String::new(),
                action_id: None,
                args: Vec::new(),
                children: vec![leaf(name, "email.bad", Vec::new())],
            }],
        };

        let error = schema.validate().expect_err("invalid child should fail");
        assert!(
            error.message().contains("invalid child action name"),
            "{name:?} produced {error}"
        );
    }
}

/// Protects the accepted command-token grammar so useful ASCII names keep
/// working while a missing prefix, whitespace, punctuation, and non-ASCII forms
/// stay out.
#[test]
fn schema_validation_documents_command_token_grammar() {
    assert!(is_valid_root_name(":a_b-1"));
    assert!(is_valid_child_name("a_b-1"));

    for root in [
        "/a_b-1",
        ":-bad",
        ":bad.name",
        ":bad/name",
        ":bad name",
        ":åbad",
    ] {
        assert!(!is_valid_root_name(root), "{root:?} should be invalid");
    }
    for child in ["-bad", "bad.name", "bad/name", "bad name", "åbad"] {
        assert!(!is_valid_child_name(child), "{child:?} should be invalid");
    }
}

/// Covers each structural schema invariant with its bounded diagnostic so
/// extension-provided metadata cannot bypass a validator branch unnoticed.
#[test]
fn schema_validation_rejects_each_structural_invariant_with_exact_diagnostic() {
    let missing_action_id = ActionCommand {
        name: ":email".to_owned(),
        description: String::new(),
        action_id: None,
        args: Vec::new(),
        children: Vec::new(),
    };
    let mut namespace_with_id = group("out", vec![leaf("list", "email.list", Vec::new())]);
    namespace_with_id.action_id = Some("email.out".to_owned());
    let mut namespace_with_args = group("out", vec![leaf("list", "email.list", Vec::new())]);
    namespace_with_args.args = vec![string_arg("value")];

    let cases = vec![
        (
            ActionSchema {
                version: ACTION_SCHEMA_VERSION + 1,
                roots: Vec::new(),
            },
            "unsupported action schema version 1; expected 0",
        ),
        (
            ActionSchema {
                version: ACTION_SCHEMA_VERSION,
                roots: vec![
                    leaf(":email", "email.one", Vec::new()),
                    leaf(":email", "email.two", Vec::new()),
                ],
            },
            "duplicate root action name `:email`",
        ),
        (
            ActionSchema {
                version: ACTION_SCHEMA_VERSION,
                roots: vec![group(
                    ":email",
                    vec![
                        leaf("list", "email.one", Vec::new()),
                        leaf("list", "email.two", Vec::new()),
                    ],
                )],
            },
            "duplicate child action name `list` in :email",
        ),
        (
            ActionSchema {
                version: ACTION_SCHEMA_VERSION,
                roots: vec![missing_action_id],
            },
            "action leaf `:email` is missing action_id",
        ),
        (
            ActionSchema {
                version: ACTION_SCHEMA_VERSION,
                roots: vec![ActionCommand {
                    name: ":email".to_owned(),
                    description: String::new(),
                    action_id: None,
                    args: Vec::new(),
                    children: vec![namespace_with_id],
                }],
            },
            "namespace action `:email out` must not set action_id",
        ),
        (
            ActionSchema {
                version: ACTION_SCHEMA_VERSION,
                roots: vec![ActionCommand {
                    name: ":email".to_owned(),
                    description: String::new(),
                    action_id: None,
                    args: Vec::new(),
                    children: vec![namespace_with_args],
                }],
            },
            "namespace action `:email out` must not declare args",
        ),
        (
            ActionSchema {
                version: ACTION_SCHEMA_VERSION,
                roots: vec![leaf(":email", " ", Vec::new())],
            },
            "invalid action_id ` ` in :email",
        ),
        (
            schema_with_leaf(vec![string_arg("bad/name")]),
            "invalid argument name `bad/name` in :test",
        ),
        (
            schema_with_leaf(vec![string_arg("value"), string_arg("value")]),
            "duplicate argument name `value` in :test",
        ),
        (
            schema_with_leaf(vec![
                arg("optional", ActionArgKind::String, false),
                string_arg("required"),
            ]),
            "required argument `required` follows an optional argument in :test",
        ),
        (
            schema_with_leaf(vec![rest_arg("rest"), string_arg("after")]),
            "rest argument `rest` must be last in :test",
        ),
        (
            schema_with_leaf(vec![enum_arg("choice", Vec::new())]),
            "enum argument `choice` in :test must declare at least one value",
        ),
        (
            schema_with_leaf(vec![enum_arg("choice", vec![choice(" ")])]),
            "invalid enum value ` ` for `choice` in :test",
        ),
        (
            schema_with_leaf(vec![enum_arg("choice", vec![choice("one"), choice("one")])]),
            "duplicate enum value `one` for `choice` in :test",
        ),
    ];

    for (schema, expected_message) in cases {
        let error = schema.validate().expect_err("malformed schema should fail");
        assert_eq!(error.message(), expected_message);
        assert_eq!(error.to_string(), expected_message);
    }
}

/// Keeps parse-time schema rejection bounded and distinct from failures in a
/// valid action invocation.
#[test]
fn parse_invalid_schema_returns_bounded_invalid_arguments_error() {
    let error = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![leaf(":email", " ", Vec::new())],
    }
    .parse_line(":email")
    .expect_err("malformed schemas should fail before parsing");

    assert_eq!(error.kind(), &ParseErrorKind::InvalidArguments);
    assert_eq!(
        error.message(),
        "invalid action schema: invalid action_id ` ` in :email"
    );
    assert_eq!(error.usage(), None);
    assert_eq!(
        error.to_string(),
        "invalid action schema: invalid action_id ` ` in :email"
    );
}

/// Proves the command-node budget accepts its exact boundary and rejects the
/// next node before an extension can publish an unbounded command tree.
#[test]
fn schema_validation_enforces_command_tree_boundary() {
    let tree = |child_count| ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![group(
            ":email",
            (0..child_count)
                .map(|index| {
                    leaf(
                        &format!("cmd{index}"),
                        &format!("email.cmd{index}"),
                        Vec::new(),
                    )
                })
                .collect(),
        )],
    };

    tree(MAX_ACTION_COMMANDS - 1)
        .validate()
        .expect("exact command-node limit should pass");
    let error = tree(MAX_ACTION_COMMANDS)
        .validate()
        .expect_err("one extra command node should fail");
    assert_eq!(
        error.message(),
        "action schema declares more than 128 command nodes"
    );
}

/// Proves every token-bearing schema field has the same inclusive byte limit
/// while retaining field-specific diagnostics for extension authors.
#[test]
fn schema_validation_enforces_token_byte_boundaries() {
    let root_at_limit = format!(":{}", "a".repeat(MAX_ACTION_TOKEN_BYTES - 1));
    let root_over_limit = format!(":{}", "a".repeat(MAX_ACTION_TOKEN_BYTES));
    let cases = vec![
        (
            ActionSchema {
                version: ACTION_SCHEMA_VERSION,
                roots: vec![leaf(&root_at_limit, "id", Vec::new())],
            },
            ActionSchema {
                version: ACTION_SCHEMA_VERSION,
                roots: vec![leaf(&root_over_limit, "id", Vec::new())],
            },
            "root action name is 129 bytes; maximum is 128",
        ),
        (
            ActionSchema {
                version: ACTION_SCHEMA_VERSION,
                roots: vec![group(
                    ":email",
                    vec![leaf(&"a".repeat(MAX_ACTION_TOKEN_BYTES), "id", Vec::new())],
                )],
            },
            ActionSchema {
                version: ACTION_SCHEMA_VERSION,
                roots: vec![group(
                    ":email",
                    vec![leaf(
                        &"a".repeat(MAX_ACTION_TOKEN_BYTES + 1),
                        "id",
                        Vec::new(),
                    )],
                )],
            },
            "child action name is 129 bytes; maximum is 128",
        ),
        (
            schema_with_leaf(vec![string_arg(&"a".repeat(MAX_ACTION_TOKEN_BYTES))]),
            schema_with_leaf(vec![string_arg(&"a".repeat(MAX_ACTION_TOKEN_BYTES + 1))]),
            "argument name is 129 bytes; maximum is 128",
        ),
        (
            ActionSchema {
                version: ACTION_SCHEMA_VERSION,
                roots: vec![leaf(
                    ":email",
                    &"a".repeat(MAX_ACTION_TOKEN_BYTES),
                    Vec::new(),
                )],
            },
            ActionSchema {
                version: ACTION_SCHEMA_VERSION,
                roots: vec![leaf(
                    ":email",
                    &"a".repeat(MAX_ACTION_TOKEN_BYTES + 1),
                    Vec::new(),
                )],
            },
            "action_id is 129 bytes; maximum is 128",
        ),
        (
            schema_with_leaf(vec![enum_arg(
                "choice",
                vec![choice("a".repeat(MAX_ACTION_TOKEN_BYTES))],
            )]),
            schema_with_leaf(vec![enum_arg(
                "choice",
                vec![choice("a".repeat(MAX_ACTION_TOKEN_BYTES + 1))],
            )]),
            "choice value is 129 bytes; maximum is 128",
        ),
    ];

    for (at_limit, over_limit, expected_message) in cases {
        at_limit
            .validate()
            .expect("token field at its byte limit should pass");
        let error = over_limit
            .validate()
            .expect_err("token field one byte over its limit should fail");
        assert_eq!(error.message(), expected_message);
    }
}

/// Keeps per-action positional argument lists bounded at their inclusive limit
/// before consumers render or route an oversized invocation schema.
#[test]
fn schema_validation_enforces_per_action_argument_boundary() {
    let args = |count| {
        (0..count)
            .map(|index| string_arg(&format!("arg{index}")))
            .collect()
    };
    schema_with_leaf(args(MAX_ACTION_ARGS))
        .validate()
        .expect("exact per-action argument limit should pass");
    let error = schema_with_leaf(args(MAX_ACTION_ARGS + 1))
        .validate()
        .expect_err("one extra action argument should fail");
    assert_eq!(
        error.message(),
        "action `:test` declares 17 arguments; maximum is 16"
    );
}

/// Keeps enum and suggestion choice lists bounded at their inclusive limit
/// through the validator shared by completion metadata.
#[test]
fn schema_validation_enforces_per_list_choice_boundary() {
    let values = |count| {
        (0..count)
            .map(|index| choice(format!("v{index}")))
            .collect()
    };
    schema_with_leaf(vec![enum_arg("choice", values(MAX_ACTION_CHOICES))])
        .validate()
        .expect("exact enum-choice limit should pass");
    let error = schema_with_leaf(vec![enum_arg("choice", values(MAX_ACTION_CHOICES + 1))])
        .validate()
        .expect_err("one extra enum choice should fail");
    assert_eq!(
        error.message(),
        "argument `choice` in :test declares 129 choices; maximum is 128"
    );

    let mut suggested = string_arg("value");
    suggested.suggestions = vec![choice(" ")];
    let error = schema_with_leaf(vec![suggested])
        .validate()
        .expect_err("invalid suggestions should use the shared choice validator");
    assert_eq!(
        error.message(),
        "invalid enum value ` ` for `value` in :test"
    );
}

/// Preserves inclusive per-field description limits and their command,
/// argument, and choice context in diagnostics.
#[test]
fn schema_validation_enforces_description_byte_boundaries() {
    let command_at_limit = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![ActionCommand {
            name: ":email".to_owned(),
            description: "a".repeat(MAX_ACTION_DESCRIPTION_BYTES),
            action_id: Some("email.action".to_owned()),
            args: Vec::new(),
            children: Vec::new(),
        }],
    };
    let command_over_limit = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![ActionCommand {
            name: ":email".to_owned(),
            description: "a".repeat(MAX_ACTION_DESCRIPTION_BYTES + 1),
            action_id: Some("email.action".to_owned()),
            args: Vec::new(),
            children: Vec::new(),
        }],
    };
    let mut argument_at_limit = string_arg("value");
    argument_at_limit.description = "a".repeat(MAX_ACTION_DESCRIPTION_BYTES);
    let mut argument_over_limit = argument_at_limit.clone();
    argument_over_limit.description.push('a');
    let mut choice_at_limit = choice("one");
    choice_at_limit.description = "a".repeat(MAX_ACTION_DESCRIPTION_BYTES);
    let mut choice_over_limit = choice_at_limit.clone();
    choice_over_limit.description.push('a');
    let cases = vec![
        (
            command_at_limit,
            command_over_limit,
            "description for :email is 1025 bytes; maximum is 1024",
        ),
        (
            schema_with_leaf(vec![argument_at_limit]),
            schema_with_leaf(vec![argument_over_limit]),
            "description for argument `value` in :test is 1025 bytes; maximum is 1024",
        ),
        (
            schema_with_leaf(vec![enum_arg("choice", vec![choice_at_limit])]),
            schema_with_leaf(vec![enum_arg("choice", vec![choice_over_limit])]),
            "description for choice `one` for `choice` in :test is 1025 bytes; maximum is 1024",
        ),
    ];

    for (at_limit, over_limit, expected_message) in cases {
        at_limit
            .validate()
            .expect("description at its byte limit should pass");
        let error = over_limit
            .validate()
            .expect_err("description one byte over its limit should fail");
        assert_eq!(error.message(), expected_message);
    }
}

/// Proves the aggregate argument budget accepts its exact total and fails only
/// after an otherwise valid leaf adds the next argument.
#[test]
fn schema_validation_enforces_aggregate_argument_boundary() {
    let args = (0..MAX_ACTION_ARGS)
        .map(|index| string_arg(&format!("arg{index}")))
        .collect::<Vec<_>>();
    let argument_schema = |leaf_count| ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![group(
            ":email",
            (0..leaf_count)
                .map(|index| {
                    leaf(
                        &format!("cmd{index}"),
                        &format!("email.cmd{index}"),
                        args.clone(),
                    )
                })
                .collect(),
        )],
    };
    argument_schema(MAX_ACTION_TOTAL_ARGS / MAX_ACTION_ARGS)
        .validate()
        .expect("exact aggregate argument limit should pass");
    let mut over_limit_args = (0..MAX_ACTION_TOTAL_ARGS / MAX_ACTION_ARGS)
        .map(|index| {
            leaf(
                &format!("cmd{index}"),
                &format!("email.cmd{index}"),
                args.clone(),
            )
        })
        .collect::<Vec<_>>();
    over_limit_args.push(leaf("extra", "email.extra", vec![string_arg("extra")]));
    let error = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![group(":email", over_limit_args)],
    }
    .validate()
    .expect_err("one extra aggregate argument should fail");
    assert_eq!(
        error.message(),
        "action schema declares more than 256 total arguments"
    );
}

/// Proves the aggregate choice budget accepts its exact total and fails only
/// after an otherwise valid enum adds the next static value.
#[test]
fn schema_validation_enforces_aggregate_choice_boundary() {
    let values = (0..MAX_ACTION_CHOICES)
        .map(|index| choice(format!("v{index}")))
        .collect::<Vec<_>>();
    let choice_schema = |last_count| {
        let mut args = (0..MAX_ACTION_TOTAL_CHOICES / MAX_ACTION_CHOICES)
            .map(|index| enum_arg(&format!("choice{index}"), values.clone()))
            .collect::<Vec<_>>();
        if 0 < last_count {
            args.push(enum_arg(
                "extra",
                (0..last_count)
                    .map(|index| choice(format!("extra{index}")))
                    .collect(),
            ));
        }
        schema_with_leaf(args)
    };
    choice_schema(0)
        .validate()
        .expect("exact aggregate choice limit should pass");
    let error = choice_schema(1)
        .validate()
        .expect_err("one extra aggregate choice should fail");
    assert_eq!(
        error.message(),
        "action schema declares more than 1024 total choices"
    );
}

/// Preserves the inclusive aggregate text budget without allowing a
/// per-description failure to mask the aggregate diagnostic.
#[test]
fn schema_validation_enforces_aggregate_text_boundary() {
    let schema_with_text_bytes = |extra: usize| {
        let mut schema = ActionSchema {
            version: ACTION_SCHEMA_VERSION,
            roots: vec![group(
                ":email",
                (0..63)
                    .map(|index| {
                        leaf(
                            &format!("cmd{index}"),
                            &format!("email.cmd{index}"),
                            Vec::new(),
                        )
                    })
                    .collect(),
            )],
        };
        schema.roots[0].description.clear();
        for child in &mut schema.roots[0].children {
            child.description.clear();
        }
        let fixed_bytes = schema.roots[0].name.len()
            + schema.roots[0]
                .children
                .iter()
                .map(|child| child.name.len() + child.action_id.as_ref().map_or(0, String::len))
                .sum::<usize>();
        let mut remaining = MAX_ACTION_TOTAL_TEXT_BYTES - fixed_bytes + extra;
        let root_bytes = remaining.min(MAX_ACTION_DESCRIPTION_BYTES);
        schema.roots[0].description = "a".repeat(root_bytes);
        remaining -= root_bytes;
        for command in &mut schema.roots[0].children {
            let bytes = remaining.min(MAX_ACTION_DESCRIPTION_BYTES);
            command.description = "a".repeat(bytes);
            remaining -= bytes;
        }
        assert_eq!(remaining, 0, "test schema needs enough description fields");
        schema
    };

    schema_with_text_bytes(0)
        .validate()
        .expect("exact aggregate text limit should pass");
    let error = schema_with_text_bytes(1)
        .validate()
        .expect_err("one extra aggregate text byte should fail");
    assert_eq!(
        error.message(),
        "action schema text exceeds 65536 total bytes while reading action ids"
    );
}

/// Verifies ordinary positional token parsing still selects the intended leaf
/// and preserves both argv and typed named-argument forms.
#[test]
fn parse_nested_action_with_positional_string_arg() {
    let parsed = email_schema()
        .parse_line(":email out approve abc-123")
        .expect("action line should parse");

    assert_eq!(
        parsed,
        ParsedAction {
            action_id: "email.out.approve".to_owned(),
            root: ":email".to_owned(),
            command_path: vec![":email".to_owned(), "out".to_owned(), "approve".to_owned(),],
            argv: vec!["abc-123".to_owned()],
            named_args: BTreeMap::from([(
                "id".to_owned(),
                ParsedArgValue::String("abc-123".to_owned()),
            )]),
        }
    );
}

/// Covers every typed positional-parser branch while preserving raw argv and
/// exact parse diagnostics for the shared extension action boundary.
#[test]
fn parse_argument_kinds_preserve_raw_argv_and_typed_values() {
    enum Expected {
        Parsed(ParsedAction),
        Error {
            message: &'static str,
            usage: &'static str,
        },
    }

    let cases = vec![
        (
            schema_with_leaf(vec![arg("count", ActionArgKind::Integer, true)]),
            ":test -42",
            Expected::Parsed(ParsedAction {
                action_id: "test.action".to_owned(),
                root: ":test".to_owned(),
                command_path: vec![":test".to_owned()],
                argv: vec!["-42".to_owned()],
                named_args: BTreeMap::from([("count".to_owned(), ParsedArgValue::Integer(-42))]),
            }),
        ),
        (
            schema_with_leaf(vec![enum_arg("color", vec![choice("red"), choice("blue")])]),
            ":test blue",
            Expected::Parsed(ParsedAction {
                action_id: "test.action".to_owned(),
                root: ":test".to_owned(),
                command_path: vec![":test".to_owned()],
                argv: vec!["blue".to_owned()],
                named_args: BTreeMap::from([(
                    "color".to_owned(),
                    ParsedArgValue::String("blue".to_owned()),
                )]),
            }),
        ),
        (
            schema_with_leaf(vec![arg("value", ActionArgKind::String, false)]),
            ":test",
            Expected::Parsed(ParsedAction {
                action_id: "test.action".to_owned(),
                root: ":test".to_owned(),
                command_path: vec![":test".to_owned()],
                argv: Vec::new(),
                named_args: BTreeMap::new(),
            }),
        ),
        (
            schema_with_leaf(vec![arg("value", ActionArgKind::String, false)]),
            ":test present",
            Expected::Parsed(ParsedAction {
                action_id: "test.action".to_owned(),
                root: ":test".to_owned(),
                command_path: vec![":test".to_owned()],
                argv: vec!["present".to_owned()],
                named_args: BTreeMap::from([(
                    "value".to_owned(),
                    ParsedArgValue::String("present".to_owned()),
                )]),
            }),
        ),
        (
            schema_with_leaf(vec![arg("count", ActionArgKind::Integer, true)]),
            ":test invalid",
            Expected::Error {
                message: "argument `count` must be an integer",
                usage: ":test <count:int>",
            },
        ),
        (
            schema_with_leaf(vec![enum_arg("color", vec![choice("red"), choice("blue")])]),
            ":test green",
            Expected::Error {
                message: "argument `color` must be one of red|blue",
                usage: ":test <red|blue>",
            },
        ),
        (
            schema_with_leaf(vec![string_arg("value")]),
            ":test value excess",
            Expected::Error {
                message: "too many arguments for :test",
                usage: ":test <value>",
            },
        ),
    ];

    for (schema, line, expected) in cases {
        match expected {
            Expected::Parsed(expected) => {
                assert_eq!(
                    schema.parse_line(line).expect("line should parse"),
                    expected
                );
            }
            Expected::Error { message, usage } => {
                let error = schema.parse_line(line).expect_err("line should fail");
                assert_eq!(error.kind(), &ParseErrorKind::InvalidArguments);
                assert_eq!(error.message(), message);
                assert_eq!(error.usage(), Some(usage));
                assert_eq!(error.to_string(), format!("{message}\nusage: {usage}"));
            }
        }
    }
}

/// Documents Tau's deliberately simple rest-string behavior: remaining
/// whitespace tokens are joined with single spaces rather than shell parsing.
#[test]
fn parse_rest_string_joins_remaining_tokens() {
    let parsed = email_schema()
        .parse_line(":email draft note hello from tau")
        .expect("rest action line should parse");

    assert_eq!(parsed.action_id, "email.draft.note");
    assert_eq!(parsed.argv, vec!["hello from tau".to_owned()]);
    assert_eq!(
        parsed.named_args.get("text"),
        Some(&ParsedArgValue::String("hello from tau".to_owned()))
    );
}

/// Keeps unknown dynamic roots distinguishable so the CLI can fall back to
/// built-in command handling when no extension owns a root.
#[test]
fn parse_unknown_root_is_distinguishable() {
    for (line, expected_message) in [
        (":missing out approve abc", "unknown action root `:missing`"),
        (" \t ", "empty action line"),
    ] {
        let error = email_schema()
            .parse_line(line)
            .expect_err("unknown or empty root should fail");

        assert_eq!(error.kind(), &ParseErrorKind::UnknownRoot);
        assert!(error.is_unknown_root());
        assert_eq!(error.message(), expected_message);
        assert_eq!(error.usage(), None);
        assert_eq!(error.to_string(), expected_message);
    }
}

/// Ensures namespace-only commands produce actionable usage instead of
/// accidentally dispatching an incomplete action invocation.
#[test]
fn parse_incomplete_namespace_reports_child_usage() {
    let error = email_schema()
        .parse_line(":email out")
        .expect_err("namespace should not execute");

    assert_eq!(error.kind(), &ParseErrorKind::IncompleteCommand);
    assert_eq!(
        error.message(),
        ":email out requires a subcommand (list, approve)"
    );
    assert_eq!(error.usage(), Some(":email out <list|approve>"));
    assert_eq!(
        error.to_string(),
        ":email out requires a subcommand (list, approve)\nusage: :email out <list|approve>"
    );
}

/// Every required argument kind shares missing-argument diagnostics. Keep all
/// branches aligned on nested paths, description inclusion, empty-description
/// fallback, and unchanged usage rendering.
#[test]
fn parse_missing_arg_descriptions_cover_every_argument_kind() {
    let choices = vec![
        ActionChoice {
            value: "one".to_owned(),
            description: String::new(),
        },
        ActionChoice {
            value: "two".to_owned(),
            description: String::new(),
        },
    ];
    let cases = [
        (
            email_schema(),
            ":email out approve",
            "missing required argument `id`: id value",
            ":email out approve <id>",
        ),
        (
            schema_with_leaf(vec![ActionArg {
                name: "value".to_owned(),
                description: "integer value".to_owned(),
                required: true,
                suggestions: Vec::new(),
                kind: ActionArgKind::Integer,
            }]),
            ":test",
            "missing required argument `value`: integer value",
            ":test <value:int>",
        ),
        (
            schema_with_leaf(vec![ActionArg {
                name: "value".to_owned(),
                description: "selected value".to_owned(),
                required: true,
                suggestions: Vec::new(),
                kind: ActionArgKind::Enum {
                    values: choices.clone(),
                },
            }]),
            ":test",
            "missing required argument `value`: selected value",
            ":test <one|two>",
        ),
        (
            schema_with_leaf(vec![ActionArg {
                name: "value".to_owned(),
                description: "remaining text".to_owned(),
                required: true,
                suggestions: Vec::new(),
                kind: ActionArgKind::RestString,
            }]),
            ":test",
            "missing required argument `value`: remaining text",
            ":test <value...>",
        ),
        (
            schema_with_leaf(vec![ActionArg {
                name: "value".to_owned(),
                description: String::new(),
                required: true,
                suggestions: Vec::new(),
                kind: ActionArgKind::String,
            }]),
            ":test",
            "missing required argument `value`",
            ":test <value>",
        ),
    ];

    for (schema, line, expected_message, expected_usage) in cases {
        let error = schema
            .parse_line(line)
            .expect_err("required argument is missing");
        assert_eq!(error.kind(), &ParseErrorKind::InvalidArguments);
        assert_eq!(error.message(), expected_message);
        assert_eq!(error.usage(), Some(expected_usage));
        assert_eq!(
            error.to_string(),
            format!("{expected_message}\nusage: {expected_usage}")
        );
    }
}
