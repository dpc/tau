use std::sync::mpsc;
use std::time::Duration;

use super::ShellCancel;

/// Ensures cancellation recorded before the watcher starts is still observed so
/// shell shutdown cannot lose an early cancellation notification.
#[test]
fn shell_cancel_observes_cancellation_before_wait() {
    let cancel = ShellCancel::default();
    cancel.cancel();
    let watcher_cancel = cancel.clone();
    let (tx, rx) = mpsc::channel();
    let watcher = std::thread::spawn(move || {
        watcher_cancel.wait_until_requested_or_completed();
        tx.send(watcher_cancel.should_report_cancel())
            .expect("test receiver should stay alive");
    });
    assert!(
        rx.recv_timeout(Duration::from_secs(1))
            .expect("cancellation watcher should wake"),
        "cancellation should be reported when no completion raced with it"
    );
    watcher.join().expect("cancellation watcher");
}

/// Ensures completion wakes a blocked cancellation watcher so ordinary shell
/// completion cannot wedge while joining the watcher thread.
#[test]
fn shell_cancel_completion_wakes_waiter() {
    let cancel = ShellCancel::default();
    let watcher_cancel = cancel.clone();
    let (tx, rx) = mpsc::channel();
    let watcher = std::thread::spawn(move || {
        watcher_cancel.wait_until_requested_or_completed();
        tx.send(watcher_cancel.should_report_cancel())
            .expect("test receiver should stay alive");
    });
    cancel.mark_completed();
    assert!(
        !rx.recv_timeout(Duration::from_secs(1))
            .expect("completion should wake cancellation watcher"),
        "completed processes should not report cancellation"
    );
    watcher.join().expect("cancellation watcher");
}
