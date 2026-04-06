//! LocalControlPlane: in-process ControlPlane for root-local services.
//!
//! This allows root-local servers to use the same `run_service()` code
//! as workers, but without any IPC — operations go directly to RootState.

#[cfg(feature = "sshd")]
use std::process::Stdio;
use std::sync::Arc;

use gateway::control_plane::{ConnectorRequest, ListenRequest, StringError};
use snafu::Snafu;

use crate::{
    listen::PerServerListener,
    root::state::{RegisterError, ServiceOwner},
};

/// In-process [`gateway::control_plane::ControlPlane`] for root-local services.
///
/// Uses the same [`RootState`](super::state::RootState) as remote workers
/// but operates directly without RPC overhead. Returns a
/// [`PerServerListener`] that implements [`h3x::quic::Listen`].
pub struct LocalControlPlane {
    state: Arc<super::state::RootState>,
}

impl LocalControlPlane {
    pub fn new(state: Arc<super::state::RootState>) -> Self {
        Self { state }
    }
}

/// Error from a local connect request.
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum LocalConnectError {
    #[snafu(display("failed to build quic client"))]
    BuildClient { source: StringError },
}

/// Error from a local session spawn.
#[cfg(feature = "sshd")]
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum LocalSpawnSessionError {
    #[snafu(display("failed to spawn session process"))]
    Spawn { source: std::io::Error },
    #[snafu(display("failed to take child stdin"))]
    TakeStdin,
    #[snafu(display("failed to take child stdout"))]
    TakeStdout,
    #[snafu(display("failed to convert child stdin to owned fd"))]
    ConvertStdinFd { source: std::io::Error },
    #[snafu(display("failed to convert child stdout to owned fd"))]
    ConvertStdoutFd { source: std::io::Error },
}

#[cfg(feature = "sshd")]
impl gateway::control_plane::SpawnSession for LocalControlPlane {
    type Error = LocalSpawnSessionError;

    async fn spawn_session(
        &self,
        username: &str,
    ) -> Result<gateway::control_plane::SessionTransport, Self::Error> {
        use local_spawn_session_error::*;
        use snafu::{OptionExt, ResultExt};

        let session_binary = crate::root::launcher::session_binary_path();

        let mut child = tokio::process::Command::new(&session_binary)
            .env("PISHOO_USER", username)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context(SpawnSnafu)?;

        let child_stdin = child.stdin.take().context(TakeStdinSnafu)?;
        let child_stdout = child.stdout.take().context(TakeStdoutSnafu)?;

        let stdin_fd = child_stdin.into_owned_fd().context(ConvertStdinFdSnafu)?;
        let stdout_fd = child_stdout.into_owned_fd().context(ConvertStdoutFdSnafu)?;

        Ok(gateway::control_plane::SessionTransport {
            stdin: stdin_fd,
            stdout: stdout_fd,
        })
    }
}

impl gateway::control_plane::ProvideListener for LocalControlPlane {
    type Listener = PerServerListener;
    type ListenError = RegisterError;

    async fn listener(&self, request: ListenRequest) -> Result<Self::Listener, Self::ListenError> {
        self.state
            .register_listener(ServiceOwner::Local, request)
            .await
    }
}

impl gateway::control_plane::ProvideConnector for LocalControlPlane {
    type Connector = Arc<dquic::prelude::QuicClient>;
    type ConnectError = LocalConnectError;

    async fn connector(
        &self,
        request: ConnectorRequest,
    ) -> Result<Self::Connector, Self::ConnectError> {
        let root_store = crate::tls::root_cert_store();
        let builder = dquic::prelude::QuicClient::builder().with_root_certificates(root_store);
        let quic_client = match request.identity {
            Some(identity) => builder
                .with_cert(identity.certs().to_vec(), identity.key().clone_key())
                .with_alpns(vec!["h3"])
                .build(),
            None => builder.without_cert().with_alpns(vec!["h3"]).build(),
        };
        Ok(Arc::new(quic_client))
    }
}
