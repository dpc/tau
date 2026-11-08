use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Wake;

use super::*;

/// Count notification edges without timing or a background polling thread.
#[derive(Default)]
struct WakeCount {
    /// Number of callbacks delivered after capacity release or closure.
    count: AtomicUsize,
}

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }
}

fn frame(text: &str) -> QueuedFrame {
    QueuedFrame::admit(
        PeerOutput::prepare(tau_proto::HarnessInputMessage::Disconnect(
            tau_proto::Disconnect {
                reason: Some(text.to_owned()),
            },
        ))
        .expect("prepare frame"),
    )
    .expect("individual frame allowance")
}

/// Frame and byte exhaustion both arm once, wake on release, and rearm when a
/// competing producer takes the newly available capacity.
#[test]
fn capacity_release_wakes_armed_producers_without_reserving_space() {
    for payload in ["small".to_owned(), "x".repeat(1024 * 1024)] {
        let queue = Arc::new(DetachedOutput::new());
        queue.activate().expect("activate");
        let token = queue.reserve_capacity().expect("reserve token");
        let wakes = Arc::new(WakeCount::default());
        token.set_waker(Waker::from(Arc::clone(&wakes)));
        while queue.admit(frame(&payload), None).is_ok() {}
        assert!(matches!(
            queue.admit_or_arm(frame(&payload), None, Some(&token)),
            Err(ClientError::Overloaded)
        ));
        assert_eq!(wakes.count.load(Ordering::SeqCst), 0);
        queue.pop().expect("release capacity");
        assert_eq!(wakes.count.load(Ordering::SeqCst), 1);
        queue
            .admit(frame(&payload), None)
            .expect("competing producer");
        assert!(matches!(
            queue.admit_or_arm(frame(&payload), None, Some(&token)),
            Err(ClientError::Overloaded)
        ));
        queue.pop().expect("release again");
        assert_eq!(wakes.count.load(Ordering::SeqCst), 2);
        queue
            .admit_or_arm(frame(&payload), None, Some(&token))
            .expect("successful retry");
        queue.pop().expect("unarmed release");
        assert_eq!(wakes.count.load(Ordering::SeqCst), 2);
    }
}

/// Closure releases an armed producer; dropping its token frees a bounded slot
/// without transferring old wake ownership to the next registration.
#[test]
fn capacity_registration_is_bounded_and_closure_wakes_once() {
    let queue = Arc::new(DetachedOutput::new());
    queue.activate().expect("activate");
    let mut tokens: Vec<_> = (0..MAX_CAPACITY_WAITERS)
        .map(|_| queue.reserve_capacity().expect("reserve"))
        .collect();
    assert!(matches!(
        queue.reserve_capacity(),
        Err(ClientError::Overloaded)
    ));
    let old = Arc::new(WakeCount::default());
    tokens[0].set_waker(Waker::from(Arc::clone(&old)));
    for _ in 0..MAX_FRAMES {
        queue.admit(frame("fill"), None).expect("fill queue");
    }
    assert!(matches!(
        queue.admit_or_arm(frame("sealed"), None, Some(&tokens[0])),
        Err(ClientError::Overloaded)
    ));
    tokens.remove(0);
    let replacement = queue.reserve_capacity().expect("reuse freed slot");
    let new = Arc::new(WakeCount::default());
    replacement.set_waker(Waker::from(Arc::clone(&new)));
    queue.pop().expect("release after replacement");
    assert_eq!(old.count.load(Ordering::SeqCst), 0);
    assert_eq!(new.count.load(Ordering::SeqCst), 0);
    queue.admit(frame("fill"), None).expect("refill");
    assert!(matches!(
        queue.admit_or_arm(frame("sealed"), None, Some(&replacement)),
        Err(ClientError::Overloaded)
    ));
    queue.close();
    queue.close();
    assert_eq!(new.count.load(Ordering::SeqCst), 1);
    assert!(matches!(
        queue.admit_or_arm(frame("retry"), None, Some(&replacement)),
        Err(ClientError::WriterClosed)
    ));
}

/// Concurrent pop and admission cannot leave a producer armed with free
/// capacity but no wake; both operations linearize under the same queue lock.
#[test]
fn capacity_release_and_arm_have_no_lost_wake_window() {
    for _ in 0..64 {
        let queue = Arc::new(DetachedOutput::new());
        queue.activate().expect("activate");
        for _ in 0..MAX_FRAMES {
            queue.admit(frame("fill"), None).expect("fill");
        }
        let token = queue.reserve_capacity().expect("reserve");
        let wakes = Arc::new(WakeCount::default());
        token.set_waker(Waker::from(Arc::clone(&wakes)));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                queue.pop().expect("release");
            });
            queue.admit_or_arm(frame("racing"), None, Some(&token))
        })
        .map_or_else(
            |error| {
                assert!(matches!(error, ClientError::Overloaded));
                assert_eq!(wakes.count.load(Ordering::SeqCst), 1);
            },
            |()| assert_eq!(wakes.count.load(Ordering::SeqCst), 0),
        );
    }
}
