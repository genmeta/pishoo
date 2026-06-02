//! Structured task scope for root-managed worker/local resources.
//!
//! A scope is a cancellation token plus a task tracker. Owners request shutdown
//! by cancelling the token, and then wait for all scoped tasks to finish. Tasks
//! are expected to observe the token and clean up their own resources; callers
//! should not abort scoped tasks during normal cleanup.

use std::{
    fmt,
    future::Future,
    sync::{Arc, Mutex},
};

#[cfg(feature = "sshd")]
use futures::future::BoxFuture;
use tokio_util::{
    sync::CancellationToken,
    task::{AbortOnDropHandle, TaskTracker},
};

#[derive(Clone)]
pub struct TaskScope {
    inner: Arc<TaskScopeInner>,
}

struct TaskScopeInner {
    token: CancellationToken,
    tasks: TaskTracker,
    handles: Mutex<Vec<AbortOnDropHandle<()>>>,
}

impl Drop for TaskScopeInner {
    fn drop(&mut self) {
        self.token.cancel();
        if let Ok(mut handles) = self.handles.lock() {
            handles.clear();
        }
    }
}

impl fmt::Debug for TaskScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskScope")
            .field("is_cancelled", &self.is_cancelled())
            .field("len", &self.len())
            .finish()
    }
}

impl TaskScope {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TaskScopeInner {
                token: CancellationToken::new(),
                tasks: TaskTracker::new(),
                handles: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn token(&self) -> CancellationToken {
        self.inner.token.clone()
    }

    pub fn len(&self) -> usize {
        self.inner.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.tasks.is_empty()
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.token.is_cancelled()
    }

    pub fn spawn<F, Fut>(&self, task: F)
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let handle = self.spawn_handle(task);
        let mut handles = self
            .inner
            .handles
            .lock()
            .expect("task scope handle registry should not be poisoned");
        handles.retain(|handle| !handle.is_finished());
        handles.push(handle);
    }

    pub fn spawn_handle<F, Fut>(&self, task: F) -> AbortOnDropHandle<()>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let token = self.token();
        AbortOnDropHandle::new(self.inner.tasks.spawn(task(token)))
    }

    pub async fn cancel_and_wait(&self) {
        self.inner.token.cancel();
        self.inner.tasks.close();
        self.inner.tasks.wait().await;
        self.inner
            .handles
            .lock()
            .expect("task scope handle registry should not be poisoned")
            .clear();
    }
}

impl Default for TaskScope {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "sshd")]
impl gateway::reverse::router::DynTaskScope for TaskScope {
    fn token(&self) -> CancellationToken {
        self.token()
    }

    fn spawn(&self, task: BoxFuture<'static, ()>) {
        self.spawn(|_| task);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::oneshot;

    use super::TaskScope;

    struct DropNotify(Option<oneshot::Sender<()>>);

    impl Drop for DropNotify {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    #[tokio::test]
    async fn dropping_scope_aborts_tracked_tasks() {
        let scope = TaskScope::new();
        let (started_tx, started_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();

        scope.spawn(|_| async move {
            let _guard = DropNotify(Some(dropped_tx));
            let _ = started_tx.send(());
            futures::future::pending::<()>().await;
        });

        started_rx.await.expect("task should start");
        drop(scope);

        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("task should be aborted when scope drops")
            .expect("drop notification should be sent");
    }
}
