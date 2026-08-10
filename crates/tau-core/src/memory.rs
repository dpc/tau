//! Snapshot-friendly in-memory connection adapter for tests and in-process
//! integrations.

use std::cell::RefCell;
use std::rc::Rc;

use tau_proto::ClientKind;

use crate::connection::{
    Connection, ConnectionOrigin, ConnectionSendError, ConnectionSink, PendingConnectionMetadata,
    RoutedFrame,
};

/// Snapshot-friendly in-memory client inbox for tests and in-process adapters.
#[derive(Clone, Debug, Default)]
pub struct MemoryInbox {
    frames: Rc<RefCell<Vec<RoutedFrame>>>,
}

impl MemoryInbox {
    /// Returns a snapshot of all delivered frames.
    #[must_use]
    pub fn snapshot(&self) -> Vec<RoutedFrame> {
        self.frames.borrow().clone()
    }

    /// Removes and returns all delivered frames.
    #[must_use]
    pub fn drain(&self) -> Vec<RoutedFrame> {
        self.frames.borrow_mut().drain(..).collect()
    }
}

#[derive(Debug)]
pub(crate) struct MemorySink {
    pub(crate) inbox: MemoryInbox,
    /// Test-adapter frames held while the modeled connection catches up.
    pending: Vec<RoutedFrame>,
    /// Whether the test adapter is inside a replay barrier.
    catch_up_paused: bool,
}

impl MemorySink {
    /// Creates an empty in-memory sink for one shared inbox.
    pub(crate) fn new(inbox: MemoryInbox) -> Self {
        Self {
            inbox,
            pending: Vec::new(),
            catch_up_paused: false,
        }
    }
}

impl ConnectionSink for MemorySink {
    fn send(&mut self, frame: RoutedFrame) -> Result<(), ConnectionSendError> {
        if self.catch_up_paused {
            self.pending.push(frame);
        } else {
            self.inbox.frames.borrow_mut().push(frame);
        }
        Ok(())
    }

    fn begin_catch_up(&mut self) {
        self.catch_up_paused = true;
    }

    fn finish_catch_up(&mut self) {
        self.catch_up_paused = false;
        self.inbox.frames.borrow_mut().append(&mut self.pending);
    }
}

/// Creates a transport-agnostic in-memory connection pair for tests.
#[must_use]
pub fn memory_connection(
    name: tau_proto::ExtensionName,
    kind: ClientKind,
) -> (Connection, MemoryInbox) {
    let inbox = MemoryInbox::default();
    let connection = Connection::new(
        PendingConnectionMetadata {
            id: None,
            name,
            kind,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(MemorySink::new(inbox.clone())),
    );
    (connection, inbox)
}
