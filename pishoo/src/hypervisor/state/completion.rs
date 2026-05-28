#![allow(dead_code)]

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone)]
pub(super) struct Completion {
    inner: Arc<CompletionInner>,
}

#[derive(Debug)]
struct CompletionInner {
    completed: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Completion {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(CompletionInner {
                completed: AtomicBool::new(false),
                notify: tokio::sync::Notify::new(),
            }),
        }
    }

    pub(super) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(super) async fn wait(&self) {
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

    pub(super) fn complete(&self) {
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
