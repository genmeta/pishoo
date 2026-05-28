//! Structured task scope for root-managed worker/local resources.
//!
//! A scope is a cancellation token plus a task tracker. Owners request shutdown
//! by cancelling the token, and then wait for all scoped tasks to finish. Tasks
//! are expected to observe the token and clean up their own resources; callers
//! should not abort scoped tasks during normal cleanup.

use std::future::Future;

#[cfg(feature = "sshd")]
use futures::future::BoxFuture;
use tokio::task::JoinHandle;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[derive(Clone, Debug)]
pub struct TaskScope {
    token: CancellationToken,
    tasks: TaskTracker,
}

impl TaskScope {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            tasks: TaskTracker::new(),
        }
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    pub fn spawn<F, Fut>(&self, task: F) -> JoinHandle<()>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let token = self.token();
        self.tasks.spawn(task(token))
    }

    pub async fn cancel_and_wait(&self) {
        self.token.cancel();
        self.tasks.close();
        self.tasks.wait().await;
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
