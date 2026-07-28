use tau_actions::{
    ACTION_SCHEMA_VERSION, ActionArg, ActionArgKind, ActionChoice, ActionCommand, ActionSchema,
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

#[test]
fn parses_known_dynamic_action_line() {
    let state = ActionCommandState::new([":quit"]);
    state.apply_schema_published(&published(":email", "email.list", 1));

    let dispatch = state
        .parse_line(":email list")
        .expect("known root")
        .expect("valid action");

    assert_eq!(
        dispatch.extension_name,
        tau_proto::ExtensionName::parse("std-email")
            .expect("test extension name must satisfy the identifier grammar")
    );
    assert_eq!(dispatch.instance_id, ExtensionInstanceId::from(1));
    assert_eq!(dispatch.parsed.action_id, "email.list");
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
