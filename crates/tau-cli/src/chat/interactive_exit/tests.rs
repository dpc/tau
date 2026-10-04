use std::cell::{Cell, RefCell};

use super::*;

/// Records lifecycle ordering without a daemon or a terminal.
#[derive(Default)]
struct ExitOracle {
    /// Hook calls and feedback, in exact dispatch order.
    steps: RefCell<Vec<String>>,
    /// Injected safe-boundary failure.
    fail: Cell<bool>,
    /// Injected harness acknowledgment for explicit detach.
    disposition: Cell<Option<tau_proto::UiQuitDisposition>>,
}

impl ExitOracle {
    /// Uses the same guarded command owner as production.
    fn run(&self, text: &str, deferred: bool) -> CommandOutcome {
        InteractiveExit {
            prepare: &|| {
                self.steps.borrow_mut().push("prepare".into());
                if self.fail.get() {
                    Err(io::Error::other("unsafe input"))
                } else {
                    Ok(())
                }
            },
            resume: &|| {
                self.steps.borrow_mut().push("resume".into());
                Ok(())
            },
            shutdown: &|| {
                self.steps.borrow_mut().push("shutdown".into());
                Ok(())
            },
            detach: &|| {
                self.steps.borrow_mut().push("detach".into());
                self.disposition.get()
            },
            disconnected: &|| false,
            feedback: &|message| self.steps.borrow_mut().push(message.into()),
            deferred,
        }
        .execute(text)
        .expect("injected exit hooks succeed")
    }
}

/// Every ordinary lifecycle operation fails before sending anything, and a
/// repeated quit remains a safe-boundary retry rather than implicit force.
#[test]
fn interactive_quit_failure_retry_precedes_all_daemon_effects() {
    for text in [":quit", ":q", ":quit-session", ":detach"] {
        let oracle = ExitOracle::default();
        oracle.fail.set(true);
        assert!(matches!(oracle.run(text, false), CommandOutcome::Continue));
        assert!(matches!(oracle.run(text, false), CommandOutcome::Continue));
        let steps = oracle.steps.borrow();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0], "prepare");
        assert_eq!(steps[2], "prepare");
        assert!(steps[1].contains("Quit cancelled"));
        assert!(steps[1].contains(":quit-force"));
        drop(steps);
        oracle.steps.borrow_mut().clear();
        oracle.fail.set(false);
        oracle
            .disposition
            .set(Some(tau_proto::UiQuitDisposition::Detached));
        assert!(matches!(oracle.run(text, false), CommandOutcome::Exit(_)));
        let steps = oracle.steps.borrow();
        assert_eq!(steps[0], "prepare");
        match text {
            ":quit-session" => assert_eq!(&*steps, &["prepare", "shutdown"]),
            ":detach" => assert_eq!(&*steps, &["prepare", "detach"]),
            _ => assert_eq!(&*steps, &["prepare"]),
        }
    }
}

/// Detach without acknowledgment restores Tau before reporting failure;
/// successful detach stays paused through teardown.
#[test]
fn interactive_detach_unconfirmed_resumes_before_feedback() {
    let oracle = ExitOracle::default();
    assert!(matches!(
        oracle.run(":detach", false),
        CommandOutcome::Continue
    ));
    let steps = oracle.steps.borrow();
    assert_eq!(&steps[..3], &["prepare", "detach", "resume"]);
    assert!(steps[3].contains("Detach was not confirmed"));
    drop(steps);
    oracle.steps.borrow_mut().clear();
    oracle
        .disposition
        .set(Some(tau_proto::UiQuitDisposition::Detached));
    assert!(matches!(
        oracle.run(":detach", false),
        CommandOutcome::Exit(InputLoopExit::Detach)
    ));
    assert_eq!(&*oracle.steps.borrow(), &["prepare", "detach"]);
}

/// Explicit force is warned and bypasses only safe preparation; preserved
/// old input cannot trigger it as a delayed automatic exit.
#[test]
fn interactive_quit_force_is_explicit_warned_and_never_deferred() {
    let oracle = ExitOracle::default();
    assert!(matches!(
        oracle.run(":quit-force", true),
        CommandOutcome::Continue
    ));
    assert!(oracle.steps.borrow()[0].contains("blocked"));
    oracle.steps.borrow_mut().clear();
    oracle.fail.set(true);
    assert!(matches!(
        oracle.run(":quit-force", false),
        CommandOutcome::Exit(InputLoopExit::Quit)
    ));
    assert_eq!(oracle.steps.borrow().len(), 1);
    assert!(oracle.steps.borrow()[0].contains("clean shell input cannot be guaranteed"));
}

/// Ctrl-D uses the same recoverable preparation without dispatching any
/// lifecycle request on failure.
#[test]
fn interactive_eof_failure_is_recoverable() {
    let feedback = RefCell::new(Vec::new());
    assert!(!prepare(
        &|| Err(io::Error::other("deadline")),
        &|message| feedback.borrow_mut().push(message.to_owned())
    ));
    assert!(feedback.borrow()[0].contains("Quit cancelled"));
    assert!(prepare(&|| Ok(()), &|_| panic!(
        "successful preparation has no failure notice"
    )));
}
