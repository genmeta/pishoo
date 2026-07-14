use std::time::Duration;

use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use tracing::Instrument;

pub const SERVICE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) trait AcceptDriver<L>: Send + Sync + 'static {
    fn drive(
        self: std::sync::Arc<Self>,
        listener: L,
        shutdown: CancellationToken,
    ) -> impl std::future::Future<Output = L> + Send;
}

pub enum DrainOutcome<L> {
    Returned(L),
    Aborted,
}

pub struct AcceptState<L> {
    shutdown: CancellationToken,
    task: AbortOnDropHandle<L>,
}

impl<L> AcceptState<L>
where
    L: Send + 'static,
{
    pub(crate) fn start<D, C>(listener: L, driver: std::sync::Arc<D>, completed: C) -> Self
    where
        D: AcceptDriver<L>,
        C: FnOnce() + Send + 'static,
    {
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(
            async move {
                let listener = driver.drive(listener, task_shutdown).await;
                completed();
                listener
            }
            .in_current_span(),
        ));
        Self { shutdown, task }
    }

    pub async fn drain(mut self) -> DrainOutcome<L> {
        self.shutdown.cancel();
        match tokio::time::timeout(SERVICE_DRAIN_TIMEOUT, &mut self.task).await {
            Ok(Ok(listener)) => DrainOutcome::Returned(listener),
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "server service task ended without returning its listener");
                DrainOutcome::Aborted
            }
            Err(_) => {
                self.task.abort();
                let _ = (&mut self.task).await;
                tracing::warn!(
                    timeout_seconds = SERVICE_DRAIN_TIMEOUT.as_secs(),
                    "server service drain timed out; task aborted"
                );
                DrainOutcome::Aborted
            }
        }
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    struct ReturningDriver;
    impl AcceptDriver<usize> for ReturningDriver {
        async fn drive(self: Arc<Self>, listener: usize, shutdown: CancellationToken) -> usize {
            shutdown.cancelled().await;
            listener
        }
    }

    struct PendingDriver {
        dropped: Arc<AtomicBool>,
    }
    struct DropListener(Arc<AtomicBool>);
    impl Drop for DropListener {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    impl AcceptDriver<DropListener> for PendingDriver {
        async fn drive(
            self: Arc<Self>,
            listener: DropListener,
            _shutdown: CancellationToken,
        ) -> DropListener {
            let _ = &self.dropped;
            let _listener = listener;
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn graceful_drain_returns_listener() {
        let state = AcceptState::start(7, Arc::new(ReturningDriver), || {});
        assert!(matches!(state.drain().await, DrainOutcome::Returned(7)));
    }

    #[tokio::test(start_paused = true)]
    async fn drain_aborts_after_exactly_five_seconds() {
        let dropped = Arc::new(AtomicBool::new(false));
        let state = AcceptState::start(
            DropListener(dropped.clone()),
            Arc::new(PendingDriver {
                dropped: dropped.clone(),
            }),
            || {},
        );
        let drain = tokio::spawn(state.drain());
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(!drain.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(matches!(drain.await.unwrap(), DrainOutcome::Aborted));
        assert!(dropped.load(Ordering::SeqCst));
    }
}
