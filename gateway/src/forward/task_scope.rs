use std::{future::Future, sync::Arc};

use tokio_util::{sync::CancellationToken, task::TaskTracker};

struct ForwardTaskInner {
    token: CancellationToken,
    tasks: TaskTracker,
}

pub(crate) struct ForwardTaskScope {
    inner: Arc<ForwardTaskInner>,
}

#[derive(Clone)]
pub(crate) struct ForwardTaskSpawner {
    inner: Arc<ForwardTaskInner>,
}

impl ForwardTaskScope {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(ForwardTaskInner {
                token: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        }
    }

    pub(crate) fn spawner(&self) -> ForwardTaskSpawner {
        ForwardTaskSpawner {
            inner: self.inner.clone(),
        }
    }

    pub(crate) async fn shutdown(self) {
        self.inner.token.cancel();
        self.inner.tasks.close();
        self.inner.tasks.wait().await;
    }
}

impl Drop for ForwardTaskScope {
    fn drop(&mut self) {
        self.inner.token.cancel();
        self.inner.tasks.close();
    }
}

impl ForwardTaskSpawner {
    pub(crate) fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let token = self.inner.token.clone();
        self.inner.tasks.spawn(async move {
            tokio::select! {
                () = token.cancelled() => {}
                () = task => {}
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    struct DropSignal {
        dropped: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
        }
    }

    #[tokio::test]
    async fn shutdown_drops_pending_spawned_task() {
        let scope = ForwardTaskScope::new();
        let spawner = scope.spawner();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();

        spawner.spawn(async move {
            let _drop_signal = DropSignal {
                dropped: Some(dropped_tx),
            };
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;

        scope.shutdown().await;

        timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("pending task should be dropped during shutdown")
            .expect("drop signal sender should fire");
    }
}
