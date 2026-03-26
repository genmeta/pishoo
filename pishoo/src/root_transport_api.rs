//! Server-side implementation of the [`RootTransportApi`] remoc RTC trait.
//!
//! Each worker process gets its own [`RootTransportApiImpl`] instance, bound to
//! the worker's PID. This allows ownership checks on `request_listen` and
//! `release_listen` calls.

use std::sync::Arc;

use nix::unistd::Pid;
use tokio::sync::Mutex;

use crate::{
    protocol::{
        ListenRequestError, OpenConnector, OpenConnectorError, ReleaseListen, ReleaseListenError,
        RequestListen, RootTransportApi,
    },
    remoc_bridge::{ConnectorHandle, ListenerHandle},
    root_state::RootState,
};

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
    ) -> Result<ListenerHandle, ListenRequestError> {
        let server_name = request.name.as_full().to_owned();

        // Phase 1: validate under lock (fast, no I/O).
        let listeners = {
            let state = self.state.lock().await;
            state.validate_listen_request(self.caller_pid, &server_name)?
        };

        // Phase 2: bind the server to the QUIC listeners (slow, involves
        // network I/O).  The state mutex is NOT held during this operation,
        // so shutdown signals and other RPCs can proceed concurrently.
        listeners
            .add_server(
                &server_name,
                request.certs.as_slice(),
                &request.key,
                request.bind,
                None::<Vec<u8>>,
            )
            .await
            .map_err(|error| {
                tracing::warn!(
                    %server_name,
                    error = %snafu::Report::from_error(&error),
                    "failed to add server to listeners"
                );
                ListenRequestError::Internal {
                    message: format!("failed to add server `{server_name}`: {error}"),
                }
            })?;

        // Phase 3: commit the registration under the lock (fast, no I/O).
        let mut state = self.state.lock().await;
        state.commit_listen_request(self.caller_pid, server_name)
    }

    async fn release_listen(&self, request: ReleaseListen) -> Result<(), ReleaseListenError> {
        let mut state = self.state.lock().await;
        state.release_listen(self.caller_pid, request)
    }

    async fn open_connector(
        &self,
        request: OpenConnector,
    ) -> Result<ConnectorHandle, OpenConnectorError> {
        let mut state = self.state.lock().await;
        state.open_connector(self.caller_pid, request).await
    }
}
