use std::cell::RefCell;
use std::rc::Rc;

use super::run_provider_auth;

/// The argument-free command keeps all chat feedback outside the terminal
/// ownership interval and preserves the built-in provider `add` arguments.
#[test]
fn provider_auth_without_argument_brackets_registration_with_feedback() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let feedback_events = events.clone();
    let registration_events = events.clone();

    run_provider_auth(
        "",
        &move |message| {
            feedback_events
                .borrow_mut()
                .push(format!("feedback:{message}"));
        },
        move |args| {
            registration_events
                .borrow_mut()
                .push(format!("registration:{}", args.join(" ")));
            Ok(())
        },
    );

    assert_eq!(
        &*events.borrow(),
        &[
            "feedback:starting provider registration; follow prompts in the terminal",
            "registration:add",
            "feedback:provider profile saved; new prompts will use updated credentials",
        ]
    );
}

/// The legacy argument form retains its warning, still invokes the same
/// interactive add flow, and reports an ordinary registration error afterward.
#[test]
fn provider_auth_with_argument_warns_before_registration_error() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let feedback_events = events.clone();
    let registration_events = events.clone();

    run_provider_auth(
        "legacy-provider",
        &move |message| {
            feedback_events
                .borrow_mut()
                .push(format!("feedback:{message}"));
        },
        move |args| {
            registration_events
                .borrow_mut()
                .push(format!("registration:{}", args.join(" ")));
            Err("injected registration failure".to_owned())
        },
    );

    assert_eq!(
        &*events.borrow(),
        &[
            "feedback:starting provider registration; follow prompts in the terminal",
            "feedback:provider arguments are no longer accepted; the add flow will prompt for provider kind and name",
            "registration:add",
            "feedback:provider registration failed: injected registration failure",
        ]
    );
}
