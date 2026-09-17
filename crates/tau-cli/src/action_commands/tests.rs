use tau_actions::{
    ACTION_SCHEMA_VERSION, ActionArg, ActionArgKind, ActionChoice, ActionCommand, ActionSchema,
    ParsedArgValue,
};

use super::*;

fn schema(root: &str, action_id: &str) -> ActionSchema {
    ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![ActionCommand {
            name: root.to_owned(),
            description: format!("{root} actions"),
            action_id: None,
            args: Vec::new(),
            children: vec![ActionCommand {
                name: "list".to_owned(),
                description: "List items".to_owned(),
                action_id: Some(action_id.to_owned()),
                args: Vec::new(),
                children: Vec::new(),
            }],
        }],
    }
}

fn published(root: &str, action_id: &str, instance_id: u64) -> ActionSchemaPublished {
    ActionSchemaPublished {
        extension_name: tau_proto::ExtensionName::parse("std-email")
            .expect("test identifier must satisfy its grammar"),
        instance_id: instance_id.into(),
        schema: schema(root, action_id),
    }
}

fn owned_publication(
    extension_name: &str,
    instance_id: u64,
    root_description: &str,
    child_name: &str,
    action_id: &str,
) -> ActionSchemaPublished {
    let mut schema = schema(":shared", action_id);
    schema.roots[0].description = root_description.to_owned();
    schema.roots[0].children[0].name = child_name.to_owned();
    ActionSchemaPublished {
        extension_name: tau_proto::ExtensionName::parse(extension_name)
            .expect("test owner name must satisfy its grammar"),
        instance_id: instance_id.into(),
        schema,
    }
}

fn nested_schema() -> ActionSchema {
    ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![ActionCommand {
            name: ":email".to_owned(),
            description: "Email approvals".to_owned(),
            action_id: None,
            args: Vec::new(),
            children: vec![
                ActionCommand {
                    name: "in".to_owned(),
                    description: "Incoming approvals".to_owned(),
                    action_id: None,
                    args: Vec::new(),
                    children: vec![
                        ActionCommand {
                            name: "open".to_owned(),
                            description: "Open incoming approval".to_owned(),
                            action_id: Some("email.in.open".to_owned()),
                            args: vec![ActionArg {
                                name: "id".to_owned(),
                                description: "Approval id".to_owned(),
                                required: true,
                                suggestions: Vec::new(),
                                kind: ActionArgKind::String,
                            }],
                            children: Vec::new(),
                        },
                        ActionCommand {
                            name: "approve".to_owned(),
                            description: "Approve incoming approvals".to_owned(),
                            action_id: Some("email.in.approve".to_owned()),
                            args: vec![ActionArg {
                                name: "ids".to_owned(),
                                description: "Approval ids".to_owned(),
                                required: true,
                                suggestions: vec![ActionChoice {
                                    value: "all".to_owned(),
                                    description: "All approvals".to_owned(),
                                }],
                                kind: ActionArgKind::RestString,
                            }],
                            children: Vec::new(),
                        },
                    ],
                },
                ActionCommand {
                    name: "out".to_owned(),
                    description: "Outgoing approvals".to_owned(),
                    action_id: None,
                    args: Vec::new(),
                    children: vec![ActionCommand {
                        name: "mode".to_owned(),
                        description: "Set outgoing mode".to_owned(),
                        action_id: Some("email.out.mode".to_owned()),
                        args: vec![ActionArg {
                            name: "mode".to_owned(),
                            description: "Mode".to_owned(),
                            required: true,
                            suggestions: Vec::new(),
                            kind: ActionArgKind::Enum {
                                values: vec![
                                    ActionChoice {
                                        value: "approve".to_owned(),
                                        description: "Approve sends".to_owned(),
                                    },
                                    ActionChoice {
                                        value: "block".to_owned(),
                                        description: "Block sends".to_owned(),
                                    },
                                ],
                            },
                        }],
                        children: Vec::new(),
                    }],
                },
            ],
        }],
    }
}

fn nested_published() -> ActionSchemaPublished {
    ActionSchemaPublished {
        extension_name: tau_proto::ExtensionName::parse("std-email")
            .expect("test identifier must satisfy its grammar"),
        instance_id: 1.into(),
        schema: nested_schema(),
    }
}

fn google_auth_published(accounts: &[&str], instance_id: u64) -> ActionSchemaPublished {
    let account_arg = ActionArg {
        name: "account".to_owned(),
        description: if accounts.is_empty() {
            "Email account id; no accounts are available".to_owned()
        } else {
            format!("Email account id; available: {}", accounts.join(", "))
        },
        required: true,
        suggestions: accounts
            .iter()
            .map(|account| ActionChoice {
                value: (*account).to_owned(),
                description: "Available Email account".to_owned(),
            })
            .collect(),
        kind: ActionArgKind::String,
    };
    ActionSchemaPublished {
        extension_name: tau_proto::ExtensionName::parse("work-pim")
            .expect("test identifier must satisfy its grammar"),
        instance_id: instance_id.into(),
        schema: ActionSchema {
            version: ACTION_SCHEMA_VERSION,
            roots: vec![ActionCommand {
                name: ":email".to_owned(),
                description: "Email actions".to_owned(),
                action_id: None,
                args: Vec::new(),
                children: vec![ActionCommand {
                    name: "auth".to_owned(),
                    description: "Authorization".to_owned(),
                    action_id: None,
                    args: Vec::new(),
                    children: vec![ActionCommand {
                        name: "google".to_owned(),
                        description: "Google authorization".to_owned(),
                        action_id: None,
                        args: Vec::new(),
                        children: vec![ActionCommand {
                            name: "start".to_owned(),
                            description: "Start authorization".to_owned(),
                            action_id: Some("email.auth.google.start".to_owned()),
                            args: vec![account_arg],
                            children: Vec::new(),
                        }],
                    }],
                }],
            }],
        },
    }
}

/// Dynamic action parsing must preserve ordered positional arguments and their
/// typed names for the `action.invoke` payload sent to the owning extension,
/// while missing required values keep the owning command and expected argument
/// actionable.
#[test]
fn parses_known_dynamic_action_line() {
    let state = ActionCommandState::new([":quit"]);
    let mut publication = published(":email", "email.list", 1);
    publication.schema.roots[0].children[0].args = vec![
        ActionArg {
            name: "mailbox".to_owned(),
            description: "Mailbox selector".to_owned(),
            required: true,
            suggestions: Vec::new(),
            kind: ActionArgKind::String,
        },
        ActionArg {
            name: "format".to_owned(),
            description: "Output format".to_owned(),
            required: true,
            suggestions: Vec::new(),
            kind: ActionArgKind::Enum {
                values: vec![ActionChoice {
                    value: "json-sentinel".to_owned(),
                    description: "Machine-readable sentinel output".to_owned(),
                }],
            },
        },
    ];
    state.apply_schema_published(&publication);

    let dispatch = state
        .parse_line(":email list mailbox-sentinel json-sentinel")
        .expect("known root")
        .expect("valid action");

    assert_eq!(
        dispatch.extension_name,
        tau_proto::ExtensionName::parse("std-email")
            .expect("test extension name must satisfy the identifier grammar")
    );
    assert_eq!(dispatch.instance_id, ExtensionInstanceId::from(1));
    assert_eq!(dispatch.parsed.action_id, "email.list");
    assert_eq!(dispatch.parsed.argv, ["mailbox-sentinel", "json-sentinel"]);
    assert_eq!(
        dispatch.parsed.named_args,
        std::collections::BTreeMap::from([
            (
                "format".to_owned(),
                ParsedArgValue::String("json-sentinel".to_owned())
            ),
            (
                "mailbox".to_owned(),
                ParsedArgValue::String("mailbox-sentinel".to_owned())
            ),
        ])
    );

    let error = state
        .parse_line(":email list")
        .expect("known root")
        .expect_err("required mailbox is missing");
    assert!(error.message().contains("mailbox"));
    assert_eq!(error.usage(), Some(":email list <mailbox> <json-sentinel>"));
}

#[test]
fn completes_dynamic_action_subcommands_and_enum_args() {
    // Extension-published action schemas are command trees, not just root
    // commands. The completer must expose nested namespaces such as
    // `:email in` and `:email out` after the root has been typed.
    let state = ActionCommandState::new([":quit"]);
    state.apply_schema_published(&nested_published());
    let data = tau_cli_term::CompletionData::new();
    let (commands, arg_completers) = state.dynamic_completions();
    data.set_dynamic_commands_and_arg_completers(commands, arg_completers);

    let labels = |buffer: &str| -> Vec<String> {
        tau_cli_term::completion::build_candidates(&[], &data, buffer, buffer.len())
            .into_iter()
            .map(|candidate| candidate.label)
            .collect()
    };

    assert_eq!(labels(":email "), vec!["in".to_owned(), "out".to_owned()]);
    assert_eq!(labels(":email i"), vec!["in".to_owned()]);
    assert_eq!(
        labels(":email in "),
        vec!["open".to_owned(), "approve".to_owned()]
    );
    assert_eq!(labels(":email in approve "), vec!["all".to_owned()]);
    assert_eq!(labels(":email out "), vec!["mode".to_owned()]);
    assert_eq!(
        labels(":email out mode "),
        vec!["approve".to_owned(), "block".to_owned()]
    );
}

/// Account suggestions published by a configured extension must reach the deep
/// action-argument position, and a replacement schema generation must remove
/// stale account names from both completion and omitted-argument errors.
#[test]
fn google_auth_account_completions_follow_latest_schema_generation() {
    let state = ActionCommandState::new([":quit"]);
    state.apply_schema_published(&google_auth_published(&["zeta", "alpha"], 7));

    let labels = |state: &ActionCommandState| {
        let data = tau_cli_term::CompletionData::new();
        let (commands, arg_completers) = state.dynamic_completions();
        data.set_dynamic_commands_and_arg_completers(commands, arg_completers);
        tau_cli_term::completion::build_candidates(
            &[],
            &data,
            ":email auth google start ",
            ":email auth google start ".len(),
        )
        .into_iter()
        .map(|candidate| candidate.label)
        .collect::<Vec<_>>()
    };

    assert_eq!(labels(&state), vec!["zeta".to_owned(), "alpha".to_owned()]);
    let error = state
        .parse_line(":email auth google start")
        .expect("known action")
        .expect_err("account is required");
    assert!(error.message().contains("zeta, alpha"));
    assert_eq!(error.usage(), Some(":email auth google start <account>"));

    state.apply_schema_published(&google_auth_published(&["current"], 7));
    assert_eq!(labels(&state), vec!["current".to_owned()]);
    let error = state
        .parse_line(":email auth google start")
        .expect("known action")
        .expect_err("account is required");
    assert!(error.message().contains("current"));
    assert!(!error.message().contains("alpha"));
}

#[test]
fn ignores_roots_that_collide_with_builtin_commands() {
    let state = ActionCommandState::new([":quit"]);
    state.apply_schema_published(&published(":quit", "quit.dynamic", 1));

    assert!(!state.is_known_action_line(":quit list"));
    assert!(state.dynamic_completions().0.is_empty());
}

/// Shared dynamic roots must select the lowest logical owner independently of
/// publication order, use that owner's schema for dispatch and completion, and
/// promote the remaining owner when the selected owner exits.
#[test]
fn shared_roots_follow_logical_owner_order_and_promote_on_removal() {
    let cases = [
        (
            ("alpha-owner", 10, "Alpha actions", "alpha", "alpha.run"),
            ("zeta-owner", 2, "Zeta actions", "zeta", "zeta.run"),
        ),
        (
            ("same-owner", 2, "Instance two", "two", "same.two"),
            ("same-owner", 10, "Instance ten", "ten", "same.ten"),
        ),
    ];

    for (winner, remaining) in cases {
        for reverse_publication_order in [false, true] {
            let state = ActionCommandState::new([":quit"]);
            let winner_publication =
                owned_publication(winner.0, winner.1, winner.2, winner.3, winner.4);
            let remaining_publication = owned_publication(
                remaining.0,
                remaining.1,
                remaining.2,
                remaining.3,
                remaining.4,
            );
            if reverse_publication_order {
                state.apply_schema_published(&remaining_publication);
                state.apply_schema_published(&winner_publication);
            } else {
                state.apply_schema_published(&winner_publication);
                state.apply_schema_published(&remaining_publication);
            }

            assert_selected_owner(&state, winner);
            state.remove_extension(
                &winner_publication.extension_name,
                winner_publication.instance_id,
            );
            assert_selected_owner(&state, remaining);
        }
    }
}

fn assert_selected_owner(state: &ActionCommandState, expected: (&str, u64, &str, &str, &str)) {
    let dispatch = state
        .parse_line(&format!(":shared {}", expected.3))
        .expect("shared root must remain selected")
        .expect("selected owner's child must parse");
    assert_eq!(dispatch.extension_name.as_ref(), expected.0);
    assert_eq!(dispatch.instance_id, ExtensionInstanceId::from(expected.1));
    assert_eq!(dispatch.parsed.action_id, expected.4);

    let data = tau_cli_term::CompletionData::new();
    let (commands, arg_completers) = state.dynamic_completions();
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].description,
        format!("{} ({})", expected.2, expected.0)
    );
    data.set_dynamic_commands_and_arg_completers(commands, arg_completers);
    let candidates =
        tau_cli_term::completion::build_candidates(&[], &data, ":shared ", ":shared ".len());
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].label, expected.3);
}

#[test]
fn removes_schema_for_exited_extension() {
    let state = ActionCommandState::new([":quit"]);
    state.apply_schema_published(&published(":email", "email.list", 2));

    state.remove_extension(
        &tau_proto::ExtensionName::parse("std-email")
            .expect("test extension name must satisfy the identifier grammar"),
        2.into(),
    );

    assert!(state.parse_line(":email list").is_none());
}
