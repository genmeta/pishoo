//! Server-side implementation of the [`RootTransportApi`] remoc RTC trait.
//!
//! Each worker process gets its own [`RootTransportApiImpl`] instance, bound to
//! the worker's PID. This allows ownership checks on `request_listen` and
//! `release_listen` calls.

use std::sync::Arc;

use h3x::remoc::quic::{ListenClient, RemoteConnectClient};
use nix::unistd::Pid;
use tokio::sync::Mutex;

use crate::protocol::{
    ListenRequestError, OpenConnector, OpenConnectorError, ReleaseListen, ReleaseListenError,
    RequestListen, RootTransportApi,
};
use crate::root_state::RootState;

/// Per-worker [`RootTransportApi`] implementation.
///
/// Created for each worker process with a fixed `caller_pid`. Delegates all
/// operations to the shared [`RootState`] behind a mutex.
pub struct RootTransportApiImpl {
    /// The PID of the worker this API instance belongs to.
    caller_pid: Pid,
    /// Shared root state, protected by an async mutex.
    state: Arc<Mutex<RootState>>,
}

impl RootTransportApiImpl {
    /// Create a new per-worker API implementation.
    pub fn new(caller_pid: Pid, state: Arc<Mutex<RootState>>) -> Self {
        Self { caller_pid, state }
    }
}

impl RootTransportApi for RootTransportApiImpl {
    async fn request_listen(
        &self,
        request: RequestListen,
    ) -> Result<ListenClient, ListenRequestError> {
        let mut state = self.state.lock().await;
        state.request_listen(self.caller_pid, request).await
    }

    async fn release_listen(
        &self,
        request: ReleaseListen,
    ) -> Result<(), ReleaseListenError> {
        let mut state = self.state.lock().await;
        state.release_listen(self.caller_pid, request)
    }

    async fn open_connector(
        &self,
        request: OpenConnector,
    ) -> Result<RemoteConnectClient, OpenConnectorError> {
        let mut state = self.state.lock().await;
        state.open_connector(self.caller_pid, request).await
    }
}
