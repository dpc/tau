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
    ActionArg {
        name: name.to_owned(),
        description: format!("{name} value"),
        required: true,
        suggestions: Vec::new(),
        kind: ActionArgKind::RestString,
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
            name: "/email".to_owned(),
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
/// publish namespace-style slash actions without parser regressions.
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
            name: "/email".to_owned(),
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

/// Keeps dynamic action roots in the slash-command namespace by requiring the
/// leading slash form used by the CLI and harness.
#[test]
fn schema_validation_rejects_invalid_root_names() {
    let schema = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![leaf("email", "email.root", Vec::new())],
    };

    let error = schema
        .validate()
        .expect_err("root without slash should fail");
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
                name: "/email".to_owned(),
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
/// working while slash, whitespace, punctuation, and non-ASCII forms stay out.
#[test]
fn schema_validation_documents_command_token_grammar() {
    assert!(is_valid_root_name("/a_b-1"));
    assert!(is_valid_child_name("a_b-1"));

    for root in ["/-bad", "/bad.name", "/bad/name", "/bad name", "/åbad"] {
        assert!(!is_valid_root_name(root), "{root:?} should be invalid");
    }
    for child in ["-bad", "bad.name", "bad/name", "bad name", "åbad"] {
        assert!(!is_valid_child_name(child), "{child:?} should be invalid");
    }
}

/// Bounds extension-provided schemas before their strings are used in
/// model-visible diagnostics, UI completions, or route tables.
#[test]
fn schema_validation_rejects_oversized_command_trees() {
    let children = (0..=MAX_ACTION_COMMANDS)
        .map(|index| {
            leaf(
                &format!("cmd{index}"),
                &format!("email.cmd{index}"),
                Vec::new(),
            )
        })
        .collect();
    let schema = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![ActionCommand {
            name: "/email".to_owned(),
            description: String::new(),
            action_id: None,
            args: Vec::new(),
            children,
        }],
    };

    let error = schema
        .validate()
        .expect_err("oversized command tree should fail");
    assert!(error.message().contains("command nodes"));
}

/// Bounds individual extension-provided fields so malformed schemas cannot
/// amplify oversized identifiers through validation errors.
#[test]
fn schema_validation_rejects_oversized_identifiers() {
    let schema = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![leaf(
            "/email",
            &"a".repeat(MAX_ACTION_TOKEN_BYTES + 1),
            Vec::new(),
        )],
    };

    let error = schema
        .validate()
        .expect_err("oversized action id should fail");
    assert!(error.message().contains("maximum"));
}

/// Rejects a single action with more positional arguments than consumers are
/// expected to render, validate, or convert into typed invocation payloads.
#[test]
fn schema_validation_rejects_per_action_argument_limit() {
    let args = (0..=MAX_ACTION_ARGS)
        .map(|index| string_arg(&format!("arg{index}")))
        .collect();
    let schema = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![leaf("/email", "email.large", args)],
    };

    let error = schema
        .validate()
        .expect_err("per-action argument limit should fail");
    assert!(error.message().contains("maximum"));
}

/// Rejects a single enum with more static choices than completion consumers are
/// expected to keep and scan.
#[test]
fn schema_validation_rejects_per_list_choice_limit() {
    let schema = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![leaf(
            "/email",
            "email.large",
            vec![ActionArg {
                name: "choice".to_owned(),
                description: String::new(),
                required: true,
                suggestions: Vec::new(),
                kind: ActionArgKind::Enum {
                    values: (0..=MAX_ACTION_CHOICES)
                        .map(|index| ActionChoice {
                            value: format!("v{index}"),
                            description: String::new(),
                        })
                        .collect(),
                },
            }],
        )],
    };

    let error = schema
        .validate()
        .expect_err("per-list choice limit should fail");
    assert!(error.message().contains("maximum"));
}

/// Rejects a single oversized description before it can appear in completion
/// menus or validation diagnostics.
#[test]
fn schema_validation_rejects_per_field_description_limit() {
    let schema = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![ActionCommand {
            name: "/email".to_owned(),
            description: "a".repeat(MAX_ACTION_DESCRIPTION_BYTES + 1),
            action_id: Some("email.large".to_owned()),
            args: Vec::new(),
            children: Vec::new(),
        }],
    };

    let error = schema
        .validate()
        .expect_err("per-field description limit should fail");
    assert!(error.message().contains("description"));
}

/// Rejects schemas whose positional arguments are individually valid but too
/// numerous in aggregate for bounded validation and routing work.
#[test]
fn schema_validation_rejects_aggregate_argument_counts() {
    let args = (0..MAX_ACTION_ARGS)
        .map(|index| string_arg(&format!("arg{index}")))
        .collect::<Vec<_>>();
    let children = (0..=(MAX_ACTION_TOTAL_ARGS / MAX_ACTION_ARGS))
        .map(|index| {
            leaf(
                &format!("cmd{index}"),
                &format!("email.cmd{index}"),
                args.clone(),
            )
        })
        .collect();
    let schema = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![group("/email", children)],
    };

    let error = schema
        .validate()
        .expect_err("aggregate argument count should fail");
    assert!(error.message().contains("total arguments"));
}

/// Rejects schemas whose enum choice lists are individually valid but too large
/// in aggregate for completion and validation consumers.
#[test]
fn schema_validation_rejects_aggregate_choice_counts() {
    let args = (0..=(MAX_ACTION_TOTAL_CHOICES / MAX_ACTION_CHOICES))
        .map(|arg_index| ActionArg {
            name: format!("arg{arg_index}"),
            description: String::new(),
            required: true,
            suggestions: Vec::new(),
            kind: ActionArgKind::Enum {
                values: (0..MAX_ACTION_CHOICES)
                    .map(|choice_index| ActionChoice {
                        value: format!("v{arg_index}_{choice_index}"),
                        description: String::new(),
                    })
                    .collect(),
            },
        })
        .collect();
    let schema = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![leaf("/email", "email.large", args)],
    };

    let error = schema
        .validate()
        .expect_err("aggregate choice count should fail");
    assert!(error.message().contains("total choices"));
}

/// Rejects schemas whose individual text fields fit per-field limits but whose
/// aggregate text would otherwise bloat diagnostics and completion state.
#[test]
fn schema_validation_rejects_aggregate_text_bytes() {
    let description = "a".repeat(MAX_ACTION_DESCRIPTION_BYTES);
    let children = (0..=(MAX_ACTION_TOTAL_TEXT_BYTES / MAX_ACTION_DESCRIPTION_BYTES))
        .map(|index| ActionCommand {
            name: format!("cmd{index}"),
            description: description.clone(),
            action_id: Some(format!("email.cmd{index}")),
            args: Vec::new(),
            children: Vec::new(),
        })
        .collect();
    let schema = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![group("/email", children)],
    };

    let error = schema
        .validate()
        .expect_err("aggregate text budget should fail");
    assert!(error.message().contains("total bytes"));
}

/// Verifies ordinary positional token parsing still selects the intended leaf
/// and preserves both argv and typed named-argument forms.
#[test]
fn parse_nested_action_with_positional_string_arg() {
    let parsed = email_schema()
        .parse_line("/email out approve abc-123")
        .expect("action line should parse");

    assert_eq!(parsed.action_id, "email.out.approve");
    assert_eq!(parsed.argv, vec!["abc-123".to_owned()]);
    assert_eq!(
        parsed.named_args.get("id"),
        Some(&ParsedArgValue::String("abc-123".to_owned()))
    );
}

/// Documents Tau's deliberately simple rest-string behavior: remaining
/// whitespace tokens are joined with single spaces rather than shell parsing.
#[test]
fn parse_rest_string_joins_remaining_tokens() {
    let parsed = email_schema()
        .parse_line("/email draft note hello from tau")
        .expect("rest action line should parse");

    assert_eq!(parsed.action_id, "email.draft.note");
    assert_eq!(parsed.argv, vec!["hello from tau".to_owned()]);
    assert_eq!(
        parsed.named_args.get("text"),
        Some(&ParsedArgValue::String("hello from tau".to_owned()))
    );
}

/// Keeps unknown dynamic roots distinguishable so the CLI can fall back to
/// built-in slash-command handling when no extension owns a root.
#[test]
fn parse_unknown_root_is_distinguishable() {
    let error = email_schema()
        .parse_line("/missing out approve abc")
        .expect_err("unknown root should fail");

    assert!(error.is_unknown_root());
}

/// Ensures namespace-only commands produce actionable usage instead of
/// accidentally dispatching an incomplete action invocation.
#[test]
fn parse_incomplete_namespace_reports_child_usage() {
    let error = email_schema()
        .parse_line("/email out")
        .expect_err("namespace should not execute");

    assert_eq!(error.kind(), &ParseErrorKind::IncompleteCommand);
    assert!(error.to_string().contains("list|approve"));
}

/// Ensures missing required positional arguments are rejected with the leaf
/// usage string that the UI can show to the user.
#[test]
fn parse_missing_arg_reports_leaf_usage() {
    let error = email_schema()
        .parse_line("/email out approve")
        .expect_err("missing id should fail");

    assert_eq!(error.kind(), &ParseErrorKind::InvalidArguments);
    assert_eq!(error.message(), "missing required argument `id`: id value");
    assert_eq!(error.usage(), Some("/email out approve <id>"));
}

/// Every required argument kind shares missing-argument diagnostics. Keep all
/// branches aligned on description inclusion, empty-description fallback, and
/// unchanged usage rendering.
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
            ActionArgKind::String,
            "text value",
            "missing required argument `value`: text value",
            "/test <value>",
        ),
        (
            ActionArgKind::Integer,
            "integer value",
            "missing required argument `value`: integer value",
            "/test <value:int>",
        ),
        (
            ActionArgKind::Enum { values: choices },
            "selected value",
            "missing required argument `value`: selected value",
            "/test <one|two>",
        ),
        (
            ActionArgKind::RestString,
            "remaining text",
            "missing required argument `value`: remaining text",
            "/test <value...>",
        ),
        (
            ActionArgKind::String,
            "",
            "missing required argument `value`",
            "/test <value>",
        ),
    ];

    for (kind, description, expected_message, expected_usage) in cases {
        let schema = ActionSchema {
            version: ACTION_SCHEMA_VERSION,
            roots: vec![leaf(
                "/test",
                "test.action",
                vec![ActionArg {
                    name: "value".to_owned(),
                    description: description.to_owned(),
                    required: true,
                    suggestions: Vec::new(),
                    kind,
                }],
            )],
        };
        let error = schema
            .parse_line("/test")
            .expect_err("required argument is missing");
        assert_eq!(error.message(), expected_message);
        assert_eq!(error.usage(), Some(expected_usage));
    }
}
