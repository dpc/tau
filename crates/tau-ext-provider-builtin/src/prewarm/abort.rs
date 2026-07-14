//! Cooperative cancellation and callback joining for one prewarm worker.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tau_provider_chatgpt::{TurnAbort, TurnAbortWaker};

/// Cancellation source owned by one supervised prewarm.
#[derive(Clone, Default)]
pub(crate) struct PrewarmAbort {
    /// Shared cancellation state and registered transport wake callbacks.
    inner: Arc<PrewarmAbortInner>,
}

#[derive(Default)]
/// Shared cancellation flag and callback registry for one prewarm worker.
struct PrewarmAbortInner {
    /// Fast authoritative cancellation flag.
    canceled: AtomicBool,
    /// Wakers registered by the current connect or response wait.
    wakers: Mutex<PrewarmWakers>,
}

#[derive(Default)]
/// Mutable callback registry protected by [`PrewarmAbortInner::wakers`].
struct PrewarmWakers {
    /// Monotonic registration identity.
    next_id: u64,
    /// Active callbacks removed when their guards drop.
    entries: HashMap<u64, Arc<dyn Fn() + Send + Sync + 'static>>,
}

impl PrewarmAbort {
    /// Cancels the work and wakes every currently registered transport wait.
    pub(crate) fn cancel(&self) {
        if let Ok(wakers) = self.inner.wakers.lock() {
            self.inner.canceled.store(true, Ordering::Release);
            // These callbacks only enqueue transport wakes or invalidate a pool
            // key; none re-enters this registry. Keeping the registry locked
            // makes guard drop a join point for an already-started callback,
            // which linearizes cancellation against staged socket release.
            for waker in wakers.entries.values() {
                waker();
            }
        } else {
            self.inner.canceled.store(true, Ordering::Release);
        }
    }

    /// Reports whether a cancellation callback currently owns the registry
    /// lock.
    #[cfg(test)]
    pub(super) fn callback_registry_is_locked(&self) -> bool {
        matches!(
            self.inner.wakers.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
    }
}

impl TurnAbort for PrewarmAbort {
    fn is_aborted(&mut self) -> bool {
        self.inner.canceled.load(Ordering::Acquire)
    }

    fn register_waker(
        &mut self,
        waker: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Box<dyn TurnAbortWaker> {
        let id = if let Ok(mut wakers) = self.inner.wakers.lock() {
            let id = wakers.next_id;
            wakers.next_id = wakers.next_id.saturating_add(1);
            wakers.entries.insert(id, Arc::clone(&waker));
            id
        } else {
            waker();
            return Box::new(PrewarmAbortWaker::detached());
        };
        if self.inner.canceled.load(Ordering::Acquire) {
            waker();
        }
        Box::new(PrewarmAbortWaker {
            inner: Some(Arc::clone(&self.inner)),
            id,
        })
    }
}

/// Registration guard that prevents callbacks leaking into later socket turns.
struct PrewarmAbortWaker {
    /// Shared callback registry, absent after a poisoned registration failure.
    inner: Option<Arc<PrewarmAbortInner>>,
    /// Callback identity to unregister.
    id: u64,
}

impl PrewarmAbortWaker {
    /// Creates an inert guard for a failed registration.
    fn detached() -> Self {
        Self { inner: None, id: 0 }
    }
}

impl Drop for PrewarmAbortWaker {
    fn drop(&mut self) {
        let Some(inner) = &self.inner else {
            return;
        };
        if let Ok(mut wakers) = inner.wakers.lock() {
            wakers.entries.remove(&self.id);
        }
    }
}

impl TurnAbortWaker for PrewarmAbortWaker {}
