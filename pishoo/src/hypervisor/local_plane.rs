//! LocalControlPlane: in-process ControlPlane for root-local services.
//!
//! This allows root-local services to use the same runtime path as workers, but
//! without any IPC — operations go directly to RootState.

#[cfg(feature = "sshd")]
use std::os::fd::AsRawFd;
#[cfg(feature = "sshd")]
use std::process::Stdio;
use std::sync::Arc;
#[cfg(feature = "sshd")]
use std::time::Duration;

use dhttp::endpoint::Endpoint;
use gateway::control_plane::{ConnectorRequest, ListenRequest};
#[cfg(feature = "sshd")]
use nix::{
    errno::Errno,
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use snafu::{ResultExt, Snafu};
#[cfg(feature = "sshd")]
use tokio_util::sync::CancellationToken;

use crate::{
    hypervisor::{
        endpoint_factory,
        state::{AcquireListenerError, RebuildListenerError, owner::Owner},
    },
    listen::RegisteredEndpoint,
};

/// In-process [`gateway::control_plane::ControlPlane`] for root-local services.
///
/// Uses the same [`RootState`](super::state::RootState) as remote workers
/// but operates directly without RPC overhead. Returns a
/// [`RegisteredEndpoint`] that implements [`h3x::quic::Listen`].
pub struct LocalControlPlane {
    state: Arc<super::state::RootState>,
}

impl LocalControlPlane {
    pub fn new(state: Arc<super::state::RootState>) -> Self {
        Self { state }
    }
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

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum LocalConnectorError {
    #[snafu(display("failed to build connector endpoint"))]
    BuildEndpoint {
        source: dhttp::endpoint::InvalidEndpointIdentityError,
    },
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
                let old_fd = std::os::fd::BorrowedFd::borrow_raw(child_raw_fd);
                crate::hypervisor::launcher::install_child_ipc_fd(old_fd)?;
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

        let child = cmd.spawn().context(SpawnSnafu)?;

        // Track the session child under the local service scope so root
        // shutdown can cancel the reaper, terminate the child, and wait until
        // it is reaped.
        self.state.spawn_local_task(move |token| async move {
            reap_local_session_child(child, token).await;
        });

        // Drop the child end in the parent; the child has it via FD 3.
        drop(child_sock);

        let parent_fd = std::os::fd::OwnedFd::from(parent_sock);

        Ok(gateway::control_plane::SessionTransport { mux_fd: parent_fd })
    }
}

#[cfg(feature = "sshd")]
async fn reap_local_session_child(mut child: tokio::process::Child, token: CancellationToken) {
    let poll_interval = Duration::from_millis(50);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::debug!(?status, "ssh-session child exited");
                return;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    error = %snafu::Report::from_error(&error),
                    "failed to poll ssh-session child"
                );
                return;
            }
        }

        tokio::select! {
            biased;
            () = token.cancelled() => {
                terminate_local_session_child(child).await;
                return;
            }
            () = tokio::time::sleep(poll_interval) => {}
        }
    }
}

#[cfg(feature = "sshd")]
async fn terminate_local_session_child(mut child: tokio::process::Child) {
    const TERMINATE_GRACE: Duration = Duration::from_secs(2);

    match child.try_wait() {
        Ok(Some(status)) => {
            tracing::debug!(?status, "ssh-session child exited");
            return;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                error = %snafu::Report::from_error(&error),
                "failed to poll ssh-session child"
            );
            return;
        }
    }

    match tokio::time::timeout(TERMINATE_GRACE, child.wait()).await {
        Ok(Ok(status)) => {
            tracing::debug!(?status, "ssh-session child exited");
            return;
        }
        Ok(Err(error)) => {
            tracing::warn!(
                error = %snafu::Report::from_error(&error),
                "failed to wait on ssh-session child"
            );
            return;
        }
        Err(_) => {}
    }

    if let Some(raw_pid) = child.id() {
        send_local_session_signal(Pid::from_raw(raw_pid as i32), Signal::SIGTERM);
    }

    match tokio::time::timeout(TERMINATE_GRACE, child.wait()).await {
        Ok(Ok(status)) => {
            tracing::debug!(?status, "ssh-session child exited");
            return;
        }
        Ok(Err(error)) => {
            tracing::warn!(
                error = %snafu::Report::from_error(&error),
                "failed to wait on ssh-session child"
            );
            return;
        }
        Err(_) => {}
    }

    tracing::warn!("ssh-session child did not exit after SIGTERM");
    if let Err(error) = child.start_kill() {
        tracing::warn!(
            error = %snafu::Report::from_error(&error),
            "failed to kill ssh-session child"
        );
        return;
    }

    match tokio::time::timeout(TERMINATE_GRACE, child.wait()).await {
        Ok(Ok(status)) => {
            tracing::debug!(?status, "ssh-session child exited");
        }
        Ok(Err(error)) => {
            tracing::warn!(
                error = %snafu::Report::from_error(&error),
                "failed to wait on ssh-session child"
            );
        }
        Err(_) => {
            tracing::warn!("ssh-session child did not reap after SIGKILL");
        }
    }
}

#[cfg(feature = "sshd")]
fn send_local_session_signal(child_pid: Pid, signal: Signal) {
    match kill(child_pid, signal) {
        Ok(()) => {
            tracing::debug!(child_pid = %child_pid, ?signal, "sent signal to ssh-session child");
        }
        Err(Errno::ESRCH) => {}
        Err(error) => {
            tracing::warn!(
                child_pid = %child_pid,
                ?signal,
                error = %snafu::Report::from_error(&error),
                "failed to signal ssh-session child"
            );
        }
    }
}

impl gateway::control_plane::ProvideListener for LocalControlPlane {
    type Listener = RegisteredEndpoint;
    type ListenError = AcquireListenerError;
    type RebuildError = RebuildListenerError;

    async fn listener(&self, request: ListenRequest) -> Result<Self::Listener, Self::ListenError> {
        self.state.acquire_listener(Owner::Local, request).await
    }

    async fn rebuild_listener(
        &self,
        _old: Self::Listener,
        request: ListenRequest,
    ) -> Result<Self::Listener, Self::RebuildError> {
        // _old is consumed and dropped; its Drop only cancels the accept
        // token, which is what we want because root destroyed the underlying
        // resource as part of the rebuild critical section. Calling shutdown
        // on it would attempt a redundant release.
        self.state.rebuild_listener(Owner::Local, request).await
    }
}

impl gateway::control_plane::ProvideConnector for LocalControlPlane {
    type Connector = Arc<Endpoint>;
    type ConnectError = LocalConnectorError;

    async fn connector(
        &self,
        request: ConnectorRequest,
    ) -> Result<Self::Connector, Self::ConnectError> {
        let endpoint = endpoint_factory::build_connector_endpoint(
            self.state.network.clone(),
            request.identity,
        )
        .await
        .context(local_connector_error::BuildEndpointSnafu)?;
        Ok(Arc::new(endpoint))
    }
}

#[cfg(all(test, feature = "sshd"))]
mod tests {
    use nix::{
        errno::Errno,
        sys::wait::{WaitPidFlag, waitpid},
        unistd::Pid,
    };
    use tokio_util::sync::CancellationToken;

    use super::reap_local_session_child;

    #[tokio::test]
    async fn local_session_reaper_terminates_and_reaps_child_on_cancel() {
        let child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep child should spawn");
        let pid = Pid::from_raw(child.id().expect("child should have pid") as i32);

        let token = CancellationToken::new();
        let task = tokio::spawn(reap_local_session_child(child, token.clone()));

        token.cancel();
        task.await.expect("reaper task should finish");

        assert!(matches!(
            waitpid(pid, Some(WaitPidFlag::WNOHANG)),
            Err(Errno::ECHILD)
        ));
    }
}
