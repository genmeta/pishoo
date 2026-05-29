use snafu::{ResultExt, Snafu};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

pub(crate) trait AcceptDriver<L>: Send + Sync + 'static {
    fn drive(
        self: std::sync::Arc<Self>,
        listener: L,
        shutdown: CancellationToken,
    ) -> impl std::future::Future<Output = L> + Send;
}

pub enum AcceptState<L> {
    Running {
        shutdown: CancellationToken,
        task: JoinHandle<L>,
    },
    Stopped {
        listener: L,
    },
    /// Transient marker held only while [`AcceptState::stop`] awaits the
    /// task's listener return. Any concurrent observer sees this and refuses
    /// to proceed; the happy path replaces it before `stop` returns.
    Transitioning,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum StopAcceptError {
    #[snafu(display("accept task panicked or was cancelled"))]
    Join { source: tokio::task::JoinError },
    #[snafu(display("accept state is in a transient stop; concurrent stop is not supported"))]
    Transitioning,
}

impl<L> AcceptState<L>
where
    L: Send + 'static,
{
    pub(crate) fn start<D>(listener: L, driver: std::sync::Arc<D>) -> Self
    where
        D: AcceptDriver<L>,
    {
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(
            async move { driver.drive(listener, task_shutdown).await }.in_current_span(),
        );
        Self::Running { shutdown, task }
    }

    pub async fn stop(&mut self) -> Result<&mut L, StopAcceptError> {
        let prev = std::mem::replace(self, Self::Transitioning);
        match prev {
            Self::Running { shutdown, task } => {
                shutdown.cancel();
                let listener = task.await.context(stop_accept_error::JoinSnafu)?;
                *self = Self::Stopped { listener };
            }
            Self::Stopped { listener } => {
                *self = Self::Stopped { listener };
            }
            Self::Transitioning => return Err(StopAcceptError::Transitioning),
        }
        match self {
            Self::Stopped { listener } => Ok(listener),
            _ => unreachable!("stop leaves Stopped"),
        }
    }

    pub async fn into_listener(mut self) -> Result<L, StopAcceptError> {
        self.stop().await?;
        match self {
            Self::Stopped { listener } => Ok(listener),
            _ => unreachable!("stop leaves Stopped"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use tokio::time::timeout;

    use super::*;

    struct FakeDriver {
        entered: AtomicBool,
    }

    struct FakeListener {
        id: usize,
    }

    impl AcceptDriver<FakeListener> for FakeDriver {
        async fn drive(
            self: Arc<Self>,
            listener: FakeListener,
            shutdown: CancellationToken,
        ) -> FakeListener {
            self.entered.store(true, Ordering::SeqCst);
            shutdown.cancelled().await;
            listener
        }
    }

    #[tokio::test]
    async fn stop_returns_listener_after_accept_task_shutdown() {
        let driver = Arc::new(FakeDriver {
            entered: AtomicBool::new(false),
        });
        let mut state = AcceptState::start(FakeListener { id: 7 }, driver.clone());

        // Yield once so the freshly-spawned task can be polled to its first
        // await point; otherwise `entered` may still read false on a
        // single-threaded runtime.
        tokio::task::yield_now().await;
        assert!(driver.entered.load(Ordering::SeqCst));

        let listener = timeout(Duration::from_secs(1), state.stop())
            .await
            .expect("stop should complete within timeout")
            .expect("stop should return listener");

        assert_eq!(listener.id, 7);
    }

    #[tokio::test]
    async fn into_listener_consumes_state() {
        let driver = Arc::new(FakeDriver {
            entered: AtomicBool::new(false),
        });
        let state = AcceptState::start(FakeListener { id: 42 }, driver);

        let listener = timeout(Duration::from_secs(1), state.into_listener())
            .await
            .expect("into_listener should complete within timeout")
            .expect("into_listener should return listener");

        assert_eq!(listener.id, 42);
    }

    #[tokio::test]
    async fn second_stop_after_completion_is_noop() {
        let driver = Arc::new(FakeDriver {
            entered: AtomicBool::new(false),
        });
        let mut state = AcceptState::start(FakeListener { id: 1 }, driver);

        let first = state.stop().await.expect("first stop").id;
        let second = state.stop().await.expect("second stop").id;
        assert_eq!(first, 1);
        assert_eq!(second, 1);
    }
}
