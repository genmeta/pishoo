use std::{future::Future, pin::Pin};

use snafu::{ResultExt, Snafu};
use tokio::sync::oneshot;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
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
        task: AbortOnDropHandle<L>,
    },
    Stopping {
        receiver: oneshot::Receiver<Result<L, tokio::task::JoinError>>,
        task: AbortOnDropHandle<()>,
    },
    Stopped {
        listener: L,
    },
    Taken,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum StopAcceptError {
    #[snafu(display("accept task panicked or was cancelled"))]
    Join { source: tokio::task::JoinError },
    #[snafu(display("accept stop transition ended before returning listener"))]
    StopTaskLost { source: oneshot::error::RecvError },
    #[snafu(display("accept listener is not available"))]
    Taken,
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
        let task = AbortOnDropHandle::new(tokio::spawn(
            async move { driver.drive(listener, task_shutdown).await }.in_current_span(),
        ));
        Self::Running { shutdown, task }
    }

    pub async fn stop(&mut self) -> Result<&mut L, StopAcceptError> {
        loop {
            match self {
                Self::Running { .. } => {
                    let prev = std::mem::replace(self, Self::Taken);
                    let Self::Running { shutdown, task } = prev else {
                        unreachable!("matched running state")
                    };
                    shutdown.cancel();
                    let (tx, receiver) = oneshot::channel();
                    let stop_task = AbortOnDropHandle::new(tokio::spawn(
                        async move {
                            let result = task.await;
                            let _ = tx.send(result);
                        }
                        .in_current_span(),
                    ));
                    *self = Self::Stopping {
                        receiver,
                        task: stop_task,
                    };
                }
                Self::Stopping { receiver, .. } => {
                    let result = std::future::poll_fn(|cx| Pin::new(&mut *receiver).poll(cx)).await;
                    match result.context(stop_accept_error::StopTaskLostSnafu)? {
                        Ok(listener) => {
                            let prev = std::mem::replace(self, Self::Stopped { listener });
                            drop(prev);
                        }
                        Err(source) => {
                            *self = Self::Taken;
                            return Err(StopAcceptError::Join { source });
                        }
                    }
                }
                Self::Stopped { listener } => return Ok(listener),
                Self::Taken => return Err(StopAcceptError::Taken),
            }
        }
    }

    pub async fn into_listener(mut self) -> Result<L, StopAcceptError> {
        self.take_listener().await
    }

    pub async fn take_listener(&mut self) -> Result<L, StopAcceptError> {
        self.stop().await?;
        let prev = std::mem::replace(self, Self::Taken);
        match self {
            Self::Taken => match prev {
                Self::Stopped { listener } => Ok(listener),
                _ => unreachable!("stop leaves Stopped before take"),
            },
            _ => unreachable!("take leaves Taken"),
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

    struct PendingDriver;

    struct PausedStopDriver {
        stop_started: tokio::sync::Notify,
        resume: tokio::sync::Notify,
    }

    struct DropSignalListener {
        dropped: Option<tokio::sync::oneshot::Sender<()>>,
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

    impl AcceptDriver<DropSignalListener> for PendingDriver {
        async fn drive(
            self: Arc<Self>,
            listener: DropSignalListener,
            _shutdown: CancellationToken,
        ) -> DropSignalListener {
            std::future::pending::<()>().await;
            listener
        }
    }

    impl AcceptDriver<FakeListener> for PausedStopDriver {
        async fn drive(
            self: Arc<Self>,
            listener: FakeListener,
            shutdown: CancellationToken,
        ) -> FakeListener {
            shutdown.cancelled().await;
            self.stop_started.notify_waiters();
            self.resume.notified().await;
            listener
        }
    }

    impl Drop for DropSignalListener {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
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
    async fn cancelled_stop_does_not_abort_accept_task() {
        let driver = Arc::new(PausedStopDriver {
            stop_started: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        });
        let mut state = AcceptState::start(FakeListener { id: 9 }, driver.clone());

        let mut stop = Box::pin(state.stop());
        tokio::select! {
            () = driver.stop_started.notified() => {}
            _ = &mut stop => panic!("stop completed before pause"),
        }
        drop(stop);

        driver.resume.notify_waiters();

        let listener = timeout(Duration::from_secs(1), state.stop())
            .await
            .expect("second stop should complete")
            .expect("second stop should return listener");
        assert_eq!(listener.id, 9);
    }

    #[tokio::test]
    async fn dropping_running_state_aborts_accept_task() {
        let driver = Arc::new(PendingDriver);
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let state = AcceptState::start(
            DropSignalListener {
                dropped: Some(dropped_tx),
            },
            driver,
        );

        drop(state);

        timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("listener should be dropped after abort")
            .expect("drop signal sender should fire");
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
