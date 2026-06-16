//! Shared primitives for asynchronous resource ownership.
//!
//! Resource state changes in pishoo run in root-tracked tasks and signal
//! waiters through [`Completion`]. Synchronous handle drops use
//! [`AsyncReleaseGuard`] to schedule exactly one asynchronous cleanup without
//! relying on numeric generations.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone)]
pub(crate) struct Completion {
    inner: Arc<CompletionInner>,
}

#[derive(Debug)]
struct CompletionInner {
    completed: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Completion {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(CompletionInner {
                completed: AtomicBool::new(false),
                notify: tokio::sync::Notify::new(),
            }),
        }
    }

    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(crate) async fn wait(&self) {
        loop {
            if self.inner.completed.load(Ordering::Acquire) {
                return;
            }

            let notified = self.inner.notify.notified();
            if self.inner.completed.load(Ordering::Acquire) {
                return;
            }

            notified.await;
        }
    }

    pub(crate) fn complete(&self) {
        if !self.inner.completed.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }
}

impl Default for Completion {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AsyncReleaseGuard {
    inner: Arc<AsyncReleaseGuardInner>,
}

#[derive(Debug)]
struct AsyncReleaseGuardInner {
    armed: AtomicBool,
}

impl AsyncReleaseGuard {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(AsyncReleaseGuardInner {
                armed: AtomicBool::new(true),
            }),
        }
    }

    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Claims cleanup responsibility for this resource handle.
    pub(crate) fn take(&self) -> bool {
        self.inner.armed.swap(false, Ordering::AcqRel)
    }

    /// Makes future drops of this handle inert because ownership moved
    /// elsewhere.
    pub(crate) fn disarm(&self) {
        self.inner.armed.store(false, Ordering::Release);
    }
}

impl Default for AsyncReleaseGuard {
    fn default() -> Self {
        Self::new()
    }
}
