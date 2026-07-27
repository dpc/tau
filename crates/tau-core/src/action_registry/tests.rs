use tau_actions::{ACTION_SCHEMA_VERSION, ActionArg, ActionArgKind, ActionCommand, ActionSchema};
use tau_proto::{ActionInvocationId, CborValue, SessionId};

use super::*;

fn schema(action_id: &str) -> ActionSchema {
    ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![ActionCommand {
            name: ":email".to_owned(),
            description: "Email approvals".to_owned(),
            action_id: None,
            args: Vec::new(),
            children: vec![ActionCommand {
                name: "out".to_owned(),
                description: "Outgoing".to_owned(),
                action_id: None,
                args: Vec::new(),
                children: vec![ActionCommand {
                    name: "approve".to_owned(),
                    description: "Approve".to_owned(),
                    action_id: Some(action_id.to_owned()),
                    args: vec![ActionArg {
                        name: "id".to_owned(),
                        description: "Approval id".to_owned(),
                        required: true,
                        suggestions: Vec::new(),
                        kind: ActionArgKind::String,
                    }],
                    children: Vec::new(),
                }],
            }],
        }],
    }
}

fn invoke(action_id: &str, instance_id: u64) -> ActionInvoke {
    ActionInvoke {
        invocation_id: ActionInvocationId::from("act-1"),
        session_id: SessionId::parse("s1").expect("known-safe SessionId must be valid"),
        extension_name: ExtensionName::from("std-email"),
        instance_id: ExtensionInstanceId::from(instance_id),
        action_id: action_id.to_owned(),
        raw_line: ":email out approve 123".to_owned(),
        argv: vec!["123".to_owned()],
        arguments: CborValue::Map(vec![(
            CborValue::Text("id".to_owned()),
            CborValue::Text("123".to_owned()),
        )]),
    }
}

#[test]
fn register_schema_routes_invocations_to_owner() {
    let mut registry = ActionRegistry::new();
    registry
        .register_schema(
            "conn-a",
            "std-email".into(),
            1.into(),
            schema("email.out.approve"),
        )
        .expect("schema should register");

    assert_eq!(
        registry.route_action_invoke(&invoke("email.out.approve", 1)),
        Ok(ConnectionId::from("conn-a"))
    );
}

#[test]
fn replacing_schema_removes_old_action_ids_for_connection() {
    let mut registry = ActionRegistry::new();
    registry
        .register_schema("conn-a", "std-email".into(), 1.into(), schema("email.old"))
        .expect("old schema should register");
    registry
        .register_schema("conn-a", "std-email".into(), 1.into(), schema("email.new"))
        .expect("new schema should register");

    assert!(
        registry
            .route_action_invoke(&invoke("email.old", 1))
            .is_err()
    );
    assert_eq!(
        registry.route_action_invoke(&invoke("email.new", 1)),
        Ok(ConnectionId::from("conn-a"))
    );
}

#[test]
fn invocation_payload_must_match_owner_schema_parse() {
    let mut registry = ActionRegistry::new();
    registry
        .register_schema(
            "conn-a",
            "std-email".into(),
            1.into(),
            schema("email.out.approve"),
        )
        .expect("schema should register");

    let mut mismatched = invoke("email.out.approve", 1);
    mismatched.raw_line = ":email out approve".to_owned();
    assert!(matches!(
        registry.route_action_invoke(&mismatched),
        Err(ActionRouteError::InvalidInvocation { .. })
    ));

    let mut mismatched = invoke("email.out.approve", 1);
    mismatched.arguments = CborValue::Map(Vec::new());
    assert!(matches!(
        registry.route_action_invoke(&mismatched),
        Err(ActionRouteError::InvalidInvocation { .. })
    ));
}

#[test]
fn duplicate_owner_action_routes_are_rejected_without_replacing_existing_owner() {
    let mut registry = ActionRegistry::new();
    registry
        .register_schema(
            "conn-a",
            "std-email".into(),
            1.into(),
            schema("email.out.approve"),
        )
        .expect("first schema should register");

    let error = registry
        .register_schema(
            "conn-b",
            "std-email".into(),
            1.into(),
            schema("email.out.approve"),
        )
        .expect_err("second owner must not steal route");
    assert!(error.message().contains("action route collision"));
    assert_eq!(
        registry.route_action_invoke(&invoke("email.out.approve", 1)),
        Ok(ConnectionId::from("conn-a"))
    );
}

#[test]
fn disconnect_unregisters_actions() {
    let mut registry = ActionRegistry::new();
    registry
        .register_schema(
            "conn-a",
            "std-email".into(),
            1.into(),
            schema("email.out.approve"),
        )
        .expect("schema should register");

    assert!(registry.unregister_connection("conn-a").is_some());
    assert!(
        registry
            .route_action_invoke(&invoke("email.out.approve", 1))
            .is_err()
    );
}

#[test]
fn invalid_schema_is_rejected_without_replacing_previous_schema() {
    let mut registry = ActionRegistry::new();
    registry
        .register_schema(
            "conn-a",
            "std-email".into(),
            1.into(),
            schema("email.out.approve"),
        )
        .expect("schema should register");
    let invalid = ActionSchema {
        version: ACTION_SCHEMA_VERSION,
        roots: vec![ActionCommand {
            name: "email".to_owned(),
            description: "missing colon".to_owned(),
            action_id: Some("email.invalid".to_owned()),
            args: Vec::new(),
            children: Vec::new(),
        }],
    };

    assert!(
        registry
            .register_schema("conn-a", "std-email".into(), 1.into(), invalid)
            .is_err()
    );
    assert_eq!(
        registry.route_action_invoke(&invoke("email.out.approve", 1)),
        Ok(ConnectionId::from("conn-a"))
    );
}
