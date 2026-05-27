use std::sync::Arc;

use axum::{Extension, extract::State, response::IntoResponse};
use dssh::{
    auth::AuthCredential,
    conversation::ipc::{IpcManageSessionStreamServerShared, IpcManageStreamAdapter},
    session::{AuthRequest, AuthenticateFn, SessionBootstrap},
};
use h3x::{qpack::field::Protocol, stream_id::StreamId};
use http::{Request, StatusCode};
use remoc::prelude::ServerShared;
use snafu::{OptionExt, Report, ResultExt, Snafu};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::Instrument;

use crate::{
    control_plane::DynSpawnSession,
    parse::types::StringList,
    reverse::{location::LocationMatch, router::RouterState},
};

/// Errors from [`run_ssh_session`].
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RunSshSessionError {
    #[snafu(display("failed to spawn session child process"))]
    SpawnSession {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[snafu(display("failed to create MuxChannel from session FD"))]
    MuxChannel { source: std::io::Error },
    #[snafu(display("failed to split MuxChannel"))]
    SplitChannel {
        source: h3x::ipc::transport::SplitError,
    },
    #[snafu(display("failed to establish remoc channel with child"))]
    RemocConnect {
        source: remoc::ConnectError<
            h3x::ipc::transport::MuxSinkError,
            h3x::ipc::transport::MuxStreamError,
        >,
    },
    #[snafu(display("failed to receive AuthenticateFn from child"))]
    RecvAuthFn { source: remoc::rch::base::RecvError },
    #[snafu(display("child did not send AuthenticateFn"))]
    NoAuthFn,
    #[snafu(display("authentication rejected by child"))]
    AuthRejected { source: dssh::session::AuthError },
    #[snafu(display("child session failed"))]
    SessionFailed {
        source: dssh::session::SessionRunError,
    },
    #[snafu(display("failed to create control stream socketpair"))]
    ControlSocketpair { source: std::io::Error },
    #[snafu(display("failed to queue control stream FD"))]
    QueueControlFd {
        source: h3x::ipc::transport::QueueFdsError,
    },
    #[snafu(display("failed to convert control stream socket to tokio"))]
    ControlFromStd { source: std::io::Error },
}

fn is_webtransport_request<B>(request: &Request<B>) -> bool {
    request
        .extensions()
        .get::<Protocol>()
        .is_some_and(|protocol| protocol.as_str() == h3x::webtransport::WEBTRANSPORT_H3)
}

fn accept_server_session_error_status(
    error: &dssh::webtransport::AcceptServerSessionError,
) -> StatusCode {
    match error {
        dssh::webtransport::AcceptServerSessionError::UnexpectedPath { .. }
        | dssh::webtransport::AcceptServerSessionError::PeerVersion { .. }
        | dssh::webtransport::AcceptServerSessionError::Accept {
            source: h3x::hyper::extended_connect::AcceptError::NotConnect { .. },
        } => StatusCode::BAD_REQUEST,
        dssh::webtransport::AcceptServerSessionError::Accept { .. }
        | dssh::webtransport::AcceptServerSessionError::RegisterSession { .. } => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Axum-style handler for DSSH WebTransport CONNECT sessions.
///
/// Extracts the username from `LocationMatch.remaining` (e.g. for `/ssh/yiyue`,
/// remaining is `"yiyue"`). Spawns the SSH session in a background task and
/// returns 200 OK with `ssh-version` header to complete the WebTransport
/// Extended CONNECT handshake.
pub async fn sshd_handle(
    Extension(loc): Extension<LocationMatch>,
    Extension(stream_id): Extension<StreamId>,
    State(state): State<RouterState>,
    req: Request<axum::body::Body>,
) -> impl IntoResponse {
    let username = loc.remaining.trim_matches('/');
    if username.is_empty() {
        tracing::warn!("missing username in SSH path");
        return StatusCode::BAD_REQUEST.into_response();
    }

    let ssh_deny = loc
        .location
        .get::<StringList>("ssh_deny")
        .ok()
        .flatten()
        .map(|value| value.0.clone())
        .unwrap_or_default();

    if ssh_deny.iter().any(|d| d == username) {
        tracing::warn!(%username, "user denied by ssh_deny");
        return StatusCode::FORBIDDEN.into_response();
    }

    let conversation_id = stream_id;
    let username = username.to_owned();

    if !is_webtransport_request(&req) {
        tracing::warn!("dssh request is not webtransport extended connect");
        return StatusCode::BAD_REQUEST.into_response();
    }

    let path = req.uri().path().to_owned();
    let accepted = match dssh::webtransport::accept_server_session(req, &path).await {
        Ok(accepted) => accepted,
        Err(error) => {
            let status = accept_server_session_error_status(&error);
            tracing::warn!(
                error = %Report::from_error(&error),
                "failed to accept dssh webtransport session"
            );
            return status.into_response();
        }
    };

    let span = tracing::info_span!("ssh-session", %conversation_id, user = %username);
    let response = accepted.response;
    tokio::spawn(
        async move {
            let manager = dssh::webtransport::WebTransportStreamManager::new(accepted.session);
            let (control_reader, control_writer) = match manager.accept_control().await {
                Ok(streams) => streams,
                Err(error) => {
                    tracing::error!(
                        error = %Report::from_error(&error),
                        "failed to accept dssh webtransport control stream"
                    );
                    return;
                }
            };

            if let Err(error) = run_ssh_session(
                &username,
                conversation_id,
                accepted.peer_version,
                manager,
                state.session_spawner.as_ref(),
                control_reader,
                control_writer,
            )
            .await
            {
                tracing::error!(error = %Report::from_error(&error), "ssh session failed");
            }
        }
        .instrument(span),
    );

    response.into_response()
}

async fn run_ssh_session<M, R, W>(
    username: &str,
    conversation_id: StreamId,
    peer_version: String,
    manage_stream: M,
    spawner: &dyn DynSpawnSession,
    control_reader: R,
    control_writer: W,
) -> Result<(), RunSshSessionError>
where
    M: dssh::conversation::ManageSessionStream + 'static,
    M::StreamReader: AsyncRead + Unpin + Send + 'static,
    M::StreamWriter: AsyncWrite + Unpin + Send + 'static,
    M::Error: Send + Sync + 'static,
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    use run_ssh_session_error::*;

    // Spawn the session child process via the control plane.
    let transport = spawner
        .spawn_session(username)
        .await
        .context(SpawnSessionSnafu)?;

    // Establish remoc channel over MuxChannel with the session child.
    let mux =
        h3x::ipc::transport::MuxChannel::from_fd(transport.mux_fd).context(MuxChannelSnafu)?;
    let (sink, stream) = mux.split().context(SplitChannelSnafu)?;

    // Capture FD sender before remoc consumes the sink.
    let fd_sender = sink.fd_sender();

    let (conn, _tx, mut rx) =
        remoc::Connect::framed::<_, _, (), AuthenticateFn, remoc::codec::Default>(
            remoc::Cfg::default(),
            sink,
            stream,
        )
        .await
        .context(RemocConnectSnafu)?;
    // Wrap in AbortOnDropHandle so an early return / panic tears down the
    // remoc connection (and its sink/stream, i.e. the socketpair) instead of
    // leaking a task that keeps the child process alive forever.
    let conn_handle =
        tokio_util::task::AbortOnDropHandle::new(tokio::spawn(conn.in_current_span()));

    let auth_fn: AuthenticateFn = rx
        .recv()
        .await
        .context(RecvAuthFnSnafu)?
        .context(NoAuthFnSnafu)?;

    let auth_request = AuthRequest {
        username: username.to_owned(),
        credential: AuthCredential::Certificate,
    };

    let start_session_fn = auth_fn
        .call(auth_request)
        .await
        .context(AuthRejectedSnafu)?;

    // Set up control stream via Unix socketpair + FD passing.
    let (ctrl_srv, ctrl_cli) =
        std::os::unix::net::UnixStream::pair().context(ControlSocketpairSnafu)?;
    ctrl_srv
        .set_nonblocking(true)
        .context(ControlFromStdSnafu)?;
    ctrl_cli
        .set_nonblocking(true)
        .context(ControlFromStdSnafu)?;
    let ctrl_fd_id = fd_sender
        .queue_fds(vec![ctrl_cli.into()].into())
        .context(QueueControlFdSnafu)?;
    let ctrl_srv = tokio::net::UnixStream::from_std(ctrl_srv).context(ControlFromStdSnafu)?;
    let (ctrl_read, ctrl_write) = ctrl_srv.into_split();

    // Bridge DSSH control stream ↔ control stream socketpair.
    tokio::spawn(
        dssh::conversation::ipc::bridge_reader_to_unix(control_reader, ctrl_write)
            .in_current_span(),
    );
    tokio::spawn(
        dssh::conversation::ipc::bridge_unix_to_writer(ctrl_read, control_writer).in_current_span(),
    );

    // Set up manage-stream RPC via IPC FD passing.
    let adapter = IpcManageStreamAdapter::new(manage_stream, fd_sender);
    let (ms, mc) = IpcManageSessionStreamServerShared::new(Arc::new(adapter), 8);
    tokio::spawn(
        async move {
            let _ = ms.serve(true).await;
        }
        .in_current_span(),
    );

    let bootstrap = SessionBootstrap {
        manage_stream: mc,
        control_fd_id: ctrl_fd_id,
        conversation_id,
        peer_version,
    };

    tracing::info!(%conversation_id, "calling StartSessionFn in child");

    let session_result = start_session_fn
        .call(bootstrap)
        .await
        .context(SessionFailedSnafu);

    // Session is done — tear down the remoc connection so the child sees
    // transport EOF on the socketpair and exits cleanly. Without this the
    // child's `conn_handle.await` would hang forever and the
    // `pishoo-ssh-session` process would linger after the SSH session ends.
    drop(_tx);
    drop(rx);
    conn_handle.abort();
    let _ = conn_handle.await;

    session_result?;
    tracing::info!(%conversation_id, "session ended");
    Ok(())
}

#[cfg(test)]
mod tests {
    use h3x::qpack::field::Protocol;

    use super::*;

    #[test]
    fn webtransport_connect_request_is_detected_by_protocol_extension() {
        let mut request = Request::builder().body(()).expect("request should build");
        request
            .extensions_mut()
            .insert(Protocol::new(h3x::webtransport::WEBTRANSPORT_H3));

        assert!(is_webtransport_request(&request));
    }

    #[test]
    fn plain_connect_request_is_not_dssh_transport() {
        let request = Request::builder().body(()).expect("request should build");

        assert!(!is_webtransport_request(&request));
    }
}
