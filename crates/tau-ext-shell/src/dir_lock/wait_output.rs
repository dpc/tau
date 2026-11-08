//! One bounded contention source, admitted independently of lock scheduling.

use std::sync::Arc;
use std::task::{Wake, Waker};

use tau_proto::{Event, HarnessInputMessage, ToolOutputReport};

use crate::Output;

/// Wake a parked publisher when detached output capacity changes or closes.
struct PublisherWake {
    /// The publisher thread, installed before its first admission attempt.
    thread: std::thread::Thread,
}

impl Wake for PublisherWake {
    fn wake(self: Arc<Self>) {
        self.thread.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.thread.unpark();
    }
}

/// Publish exactly one report, retrying only on registered capacity wakes.
///
/// The caller runs this on a scoped worker, outside all lock-manager guards.
/// Joining it before terminal publication preserves source-before-terminal
/// order even when acquisition or cancellation wins while output is saturated.
pub(super) fn publish(
    output: &Output,
    capacity: Option<tau_client::DetachedCapacityToken>,
    report: ToolOutputReport,
) {
    if let Some(token) = &capacity {
        token.set_waker(Waker::from(Arc::new(PublisherWake {
            thread: std::thread::current(),
        })));
    }
    loop {
        let message = HarnessInputMessage::emit_with_persist(
            Event::ToolOutputReported(report.clone()),
            false,
        );
        match output.send_or_arm(message, capacity.as_ref()) {
            Ok(()) => return,
            Err(tau_client::ClientError::Overloaded) => std::thread::park(),
            Err(error) => {
                let _ = output.retain_mandatory_failure(Err(error));
                return;
            }
        }
    }
}
