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
}

impl ConnectionSink for MemorySink {
    fn send(&mut self, frame: RoutedFrame) -> Result<(), ConnectionSendError> {
        self.inbox.frames.borrow_mut().push(frame);
        Ok(())
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
        Box::new(MemorySink {
            inbox: inbox.clone(),
        }),
    );
    (connection, inbox)
}
