//! Checked output and worker-to-loop failure propagation.

#[cfg(test)]
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use tau_client::{ClientError, ClientHandle, ClientResult, ManualRuntimeWaker};

#[cfg(test)]
static SATURATION_HOOK: Mutex<Option<(tau_proto::ToolCallId, Sender<()>)>> = Mutex::new(None);

/// Cloneable checked output for correctness-critical protocol frames.
#[derive(Clone)]
pub(crate) struct MandatoryOutput {
    /// Production client handle that performs ordered writes.
    handle: Option<ClientHandle>,
    /// First output failure and the loop wake handle.
    failure: Arc<Mutex<Failure>>,
}

/// Shared state used to wake the protocol loop after a worker write fails.
#[derive(Default)]
struct Failure {
    /// First failure retained until the protocol loop observes it.
    message: Option<String>,
    /// Manual-loop wake handle installed after startup.
    waker: Option<ManualRuntimeWaker>,
}

impl MandatoryOutput {
    /// Construct checked output around the production client handle.
    pub(crate) fn new(handle: ClientHandle) -> Self {
        Self {
            handle: Some(handle),
            failure: Arc::default(),
        }
    }

    /// Construct output without a writer for state-only unit tests.
    #[cfg(test)]
    pub(crate) fn disconnected() -> Self {
        Self {
            handle: None,
            failure: Arc::default(),
        }
    }

    /// Fill the real detached FIFO before a test-only mandatory write.
    #[cfg(test)]
    pub(crate) fn saturate_detached_for_test(&self) {
        let notify = SATURATION_HOOK
            .lock()
            .expect("saturation notification lock")
            .as_ref()
            .map(|(_, notify)| notify.clone());
        let Some(notify) = notify else {
            return;
        };
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        for _ in 0..96 {
            match handle.emit_transient_detached(tau_proto::Event::TermBell(tau_proto::TermBell {}))
            {
                Err(ClientError::Overloaded) => {
                    let _ = notify.send(());
                    return;
                }
                Ok(()) => {}
                Err(_) => return,
            }
        }
    }

    /// Install the deterministic production-FIFO saturation notification.
    #[cfg(test)]
    pub(crate) fn install_saturation_notify(call_id: tau_proto::ToolCallId, notify: Sender<()>) {
        *SATURATION_HOOK
            .lock()
            .expect("saturation notification lock") = Some((call_id, notify));
    }

    /// Remove the deterministic production-FIFO saturation notification.
    #[cfg(test)]
    pub(crate) fn clear_saturation_notify() {
        SATURATION_HOOK
            .lock()
            .expect("saturation notification lock")
            .take();
    }

    /// Install the wake handle used after an asynchronous write failure.
    pub(crate) fn install_waker(&self, waker: ManualRuntimeWaker) {
        self.failure
            .lock()
            .expect("mandatory output failure lock")
            .waker = Some(waker);
    }

    /// Submit one terminal outcome and retain any writer failure for the loop.
    pub(crate) fn report_tool_terminal(
        &self,
        outcome: tau_client::ToolTerminalOutcome,
    ) -> ClientResult<()> {
        #[cfg(test)]
        let call_id = match &outcome {
            tau_client::ToolTerminalOutcome::Result(result) => &result.call_id,
            tau_client::ToolTerminalOutcome::Failure(error) => &error.call_id,
            tau_client::ToolTerminalOutcome::Cancelled(cancelled) => &cancelled.call_id,
        };
        #[cfg(test)]
        if SATURATION_HOOK
            .lock()
            .expect("saturation notification lock")
            .as_ref()
            .is_some_and(|(target, _)| target == call_id)
        {
            self.saturate_detached_for_test();
        }
        let result = self
            .handle
            .as_ref()
            .map_or(Err(ClientError::WriterClosed), |handle| {
                handle.report_tool_terminal(outcome)
            });
        self.retain(result)
    }

    /// Return the first asynchronous writer failure.
    pub(crate) fn take_failure(&self) -> ClientResult<()> {
        let message = self
            .failure
            .lock()
            .expect("mandatory output failure lock")
            .message
            .take();
        match message {
            Some(message) => Err(ClientError::handler(message)),
            None => Ok(()),
        }
    }

    /// Record a checked-write failure and wake the protocol loop.
    fn retain(&self, result: ClientResult<()>) -> ClientResult<()> {
        if let Err(error) = &result {
            let waker = {
                let mut failure = self.failure.lock().expect("mandatory output failure lock");
                failure.message.get_or_insert_with(|| error.to_string());
                failure.waker.clone()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
        result
    }
}
