use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::mpsc::Sender as TestSender;
use std::sync::{Arc, Mutex, MutexGuard};

/// Shared admission flag and mutation drain for one worker generation.
struct WorkerAuthority {
    /// False as soon as worker termination begins.
    accepting: AtomicBool,
    /// Serializes retirement with complete tool mutations.
    mutation: Mutex<()>,
}

/// Authoritative publication health for one owned Swarm worker generation.
#[derive(Clone)]
pub(crate) struct WorkerHealth {
    /// Shared generation admission state and complete-mutation drain.
    authority: Arc<WorkerAuthority>,
}

impl WorkerHealth {
    /// Creates an indeterminate health state with no live publisher.
    pub(crate) fn indeterminate() -> Self {
        Self {
            authority: Arc::new(WorkerAuthority {
                accepting: AtomicBool::new(false),
                mutation: Mutex::new(()),
            }),
        }
    }

    /// Creates health for a worker generation about to enter its owned task.
    pub(crate) fn running() -> Self {
        Self {
            authority: Arc::new(WorkerAuthority {
                accepting: AtomicBool::new(true),
                mutation: Mutex::new(()),
            }),
        }
    }

    /// Returns whether this generation still has a live publisher.
    #[cfg(test)]
    pub(crate) fn is_running(&self) -> bool {
        self.authority.accepting.load(Ordering::Acquire)
    }

    /// Locks publication authority through one complete tool mutation.
    pub(crate) fn mutation_authority(&self) -> Result<MutationAuthority<'_>, String> {
        if !self.authority.accepting.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        let authority = self
            .authority
            .mutation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.authority.accepting.load(Ordering::Acquire) {
            Ok(MutationAuthority {
                _authority: authority,
            })
        } else {
            Err(unavailable())
        }
    }

    /// Creates a terminal guard that reports admission close and completed
    /// drain.
    #[cfg(test)]
    pub(crate) fn terminal_guard_notifying(
        &self,
        admission_closed: TestSender<()>,
        drained: TestSender<()>,
    ) -> WorkerTerminalGuard {
        WorkerTerminalGuard {
            health: self.clone(),
            admission_closed: Some(admission_closed),
            drained: Some(drained),
        }
    }

    /// Marks this worker generation terminal before any optional reporting.
    fn mark_indeterminate(&self) {
        self.mark_indeterminate_with(|| {}, || {});
    }

    /// Runs the single retirement sequence with boundary observers.
    fn mark_indeterminate_with(&self, admission_closed: impl FnOnce(), drained: impl FnOnce()) {
        self.authority.accepting.store(false, Ordering::Release);
        admission_closed();
        let _drained = self
            .authority
            .mutation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        drained();
    }

    /// Creates a panic-safe terminal transition owned by the worker task.
    pub(crate) fn terminal_guard(&self) -> WorkerTerminalGuard {
        WorkerTerminalGuard {
            health: self.clone(),
            #[cfg(test)]
            admission_closed: None,
            #[cfg(test)]
            drained: None,
        }
    }
}

/// Opaque authority held across one complete mutating tool operation.
pub(crate) struct MutationAuthority<'a> {
    /// Private generation lock whose lifetime covers the mutation.
    _authority: MutexGuard<'a, ()>,
}

/// Panic-safe worker-task guard that retires publication authority on drop.
pub(crate) struct WorkerTerminalGuard {
    /// Health generation retired when the worker returns or unwinds.
    health: WorkerHealth,
    /// Test-only admission-close barrier.
    #[cfg(test)]
    admission_closed: Option<TestSender<()>>,
    /// Test-only completed-drain barrier.
    #[cfg(test)]
    drained: Option<TestSender<()>>,
}

impl Drop for WorkerTerminalGuard {
    fn drop(&mut self) {
        #[cfg(test)]
        if let (Some(admission_closed), Some(drained)) =
            (self.admission_closed.take(), self.drained.take())
        {
            self.health.mark_indeterminate_with(
                || admission_closed.send(()).expect("admission-close signal"),
                || drained.send(()).expect("mutation-drain signal"),
            );
            return;
        }
        self.health.mark_indeterminate();
    }
}

fn unavailable() -> String {
    "Tau Swarm owner is unavailable until successful replay has a live publication worker".into()
}
