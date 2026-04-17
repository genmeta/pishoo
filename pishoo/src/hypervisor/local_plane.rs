//! LocalControlPlane: in-process ControlPlane for root-local services.
//!
//! This allows root-local servers to use the same `run_service()` code
//! as workers, but without any IPC — operations go directly to RootState.

#[cfg(feature = "sshd")]
use std::os::fd::AsRawFd;
#[cfg(feature = "sshd")]
use std::process::Stdio;
use std::sync::Arc;

use gateway::control_plane::{ConnectorRequest, ListenRequest, StringError};
use snafu::Snafu;

use crate::{
    hypervisor::state::{RegisterError, ServiceOwner},
    listen::PerServerListener,
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
    #[snafu(display("failed to create socketpair"))]
    CreateSocketpair { source: std::io::Error },
    #[snafu(display("failed to spawn session process"))]
    Spawn { source: std::io::Error },
}

#[cfg(feature = "sshd")]
impl gateway::control_plane::SpawnSession for LocalControlPlane {
    type Error = LocalSpawnSessionError;

    async fn spawn_session(
        &self,
        username: &str,
    ) -> Result<gateway::control_plane::SessionTransport, Self::Error> {
        use local_spawn_session_error::*;
        use snafu::ResultExt;

        let session_binary = crate::hypervisor::launcher::session_binary_path();

        // Create a Unix socketpair for MuxChannel transport.
        let (parent_sock, child_sock) =
            std::os::unix::net::UnixStream::pair().context(CreateSocketpairSnafu)?;

        let child_raw_fd = child_sock.as_raw_fd();

        let mut cmd = tokio::process::Command::new(&session_binary);
        cmd.env("PISHOO_USER", username)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());

        // In pre_exec: dup2 the child socketpair end to FD 3, then close
        // all FDs >= 4 (same as launcher.rs session_child_exec).
        unsafe {
            cmd.pre_exec(move || {
                use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd};

                if child_raw_fd != 3 {
                    let old_fd = BorrowedFd::borrow_raw(child_raw_fd);
                    let mut fd3 = OwnedFd::from_raw_fd(3);
                    nix::unistd::dup2(old_fd, &mut fd3)?;
                    std::mem::forget(fd3);
                }
                // Close FDs from 4 upwards.
                let max_fd = nix::unistd::sysconf(nix::unistd::SysconfVar::OPEN_MAX)
                    .ok()
                    .flatten()
                    .unwrap_or(1024) as i32;
                for fd in 4..max_fd {
                    let _ = nix::unistd::close(fd);
                }
                Ok(())
            });
        }

        let _child = cmd.spawn().context(SpawnSnafu)?;

        // Drop the child end in the parent; the child has it via FD 3.
        drop(child_sock);

        let parent_fd = std::os::fd::OwnedFd::from(parent_sock);

        Ok(gateway::control_plane::SessionTransport { mux_fd: parent_fd })
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
    type Connector = Arc<h3x::dquic::prelude::QuicClient>;
    type ConnectError = LocalConnectError;

    async fn connector(
        &self,
        request: ConnectorRequest,
    ) -> Result<Self::Connector, Self::ConnectError> {
        let root_store = crate::tls::root_cert_store();
        let builder = h3x::dquic::prelude::QuicClient::builder().with_root_certificates(root_store);
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
