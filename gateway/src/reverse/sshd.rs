use std::sync::Arc;

use axum::{Extension, extract::State, response::IntoResponse};
use dssh::{
    auth::AuthCredential,
    conversation::ipc::{IpcManageSessionStreamServerShared, IpcManageStreamAdapter},
    session::{AuthRequest, AuthenticateFn, AuthenticatedSession, SessionBootstrap},
};
use h3x::{qpack::field::Protocol, stream_id::StreamId};
use http::{Request, StatusCode};
use remoc::prelude::ServerShared;
use snafu::{OptionExt, Report, ResultExt, Snafu};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
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
    #[snafu(display("ssh session cancelled"))]
    Cancelled,
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
    #[snafu(display("remoc channel with child terminated"))]
    RemocConnection {
        source: remoc::chmux::ChMuxError<
            h3x::ipc::transport::MuxSinkError,
            h3x::ipc::transport::MuxStreamError,
        >,
    },
    #[snafu(display("remoc channel with child closed"))]
    RemocClosed,
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
    #[snafu(display("failed to deliver control stream FD"))]
    DeliverControlFd {
        source: h3x::ipc::transport::DeliverFdsError,
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
    let session_spawner = state.session_spawner.clone();
    let task_scope = state.task_scope.clone();
    let task_token = task_scope.token();
    task_scope.spawn(Box::pin(
        async move {
            let manager = dssh::webtransport::WebTransportStreamManager::new(accepted.session);
            let (control_reader, control_writer) = match tokio::select! {
                () = task_token.cancelled() => {
                    tracing::debug!("ssh session cancelled before control stream was accepted");
                    return;
                }
                result = manager.accept_control() => result,
            } {
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
                AcceptedSshSession {
                    peer_version: accepted.peer_version,
                    manage_stream: manager,
                    control_reader,
                    control_writer,
                },
                session_spawner.as_ref(),
                task_token,
            )
            .await
            {
                match error {
                    RunSshSessionError::Cancelled => {
                        tracing::debug!("ssh session cancelled");
                    }
                    error => {
                        tracing::error!(error = %Report::from_error(&error), "ssh session failed");
                    }
                }
            }
        }
        .instrument(span),
    ));

    response.into_response()
}

struct AcceptedSshSession<M, R, W> {
    peer_version: String,
    manage_stream: M,
    control_reader: R,
    control_writer: W,
}

async fn run_ssh_session<M, R, W>(
    username: &str,
    conversation_id: StreamId,
    accepted: AcceptedSshSession<M, R, W>,
    spawner: &dyn DynSpawnSession,
    token: CancellationToken,
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

    let AcceptedSshSession {
        peer_version,
        manage_stream,
        control_reader,
        control_writer,
    } = accepted;

    // Spawn the session child process via the control plane.
    let transport = tokio::select! {
        () = token.cancelled() => return CancelledSnafu.fail(),
        result = spawner.spawn_session(username) => result.context(SpawnSessionSnafu)?,
    };

    // Establish remoc channel over MuxChannel with the session child.
    let mux =
        h3x::ipc::transport::MuxChannel::from_fd(transport.mux_fd).context(MuxChannelSnafu)?;
    let (sink, stream) = mux.split().context(SplitChannelSnafu)?;

    // Capture the FD transfer plane before remoc consumes the transport.
    let fd_transfer = stream.fd_transfer(sink.fd_sender());

    let connect = remoc::Connect::framed::<_, _, (), AuthenticateFn, remoc::codec::Default>(
        remoc::Cfg::default(),
        sink,
        stream,
    );
    let (conn, tx, mut rx) = tokio::select! {
        () = token.cancelled() => return CancelledSnafu.fail(),
        result = connect => result.context(RemocConnectSnafu)?,
    };
    let mut conn = Box::pin(conn.in_current_span());

    let auth_fn: AuthenticateFn = tokio::select! {
        () = token.cancelled() => return CancelledSnafu.fail(),
        result = &mut conn => {
            result.context(RemocConnectionSnafu)?;
            return RemocClosedSnafu.fail();
        }
        result = rx.recv() => result,
    }
    .context(RecvAuthFnSnafu)?
    .context(NoAuthFnSnafu)?;

    let auth_request = AuthRequest {
        username: username.to_owned(),
        credential: AuthCredential::Certificate,
    };

    let authenticated: AuthenticatedSession = tokio::select! {
        () = token.cancelled() => return CancelledSnafu.fail(),
        result = &mut conn => {
            result.context(RemocConnectionSnafu)?;
            return RemocClosedSnafu.fail();
        }
        result = auth_fn.call(auth_request) => result.context(AuthRejectedSnafu)?,
    };

    // Set up control stream via Unix socketpair + FD passing.
    let (ctrl_srv, ctrl_cli) =
        std::os::unix::net::UnixStream::pair().context(ControlSocketpairSnafu)?;
    ctrl_srv
        .set_nonblocking(true)
        .context(ControlFromStdSnafu)?;
    ctrl_cli
        .set_nonblocking(true)
        .context(ControlFromStdSnafu)?;
    let mut fds = h3x::ipc::transport::FdVec::new();
    fds.push(ctrl_cli.into());
    let delivery = fd_transfer
        .delivery(authenticated.control_fd_id)
        .deliver(fds);
    tokio::pin!(delivery);
    let ctrl_srv = tokio::net::UnixStream::from_std(ctrl_srv).context(ControlFromStdSnafu)?;
    let (ctrl_read, ctrl_write) = ctrl_srv.into_split();

    let session_shutdown = token.child_token();
    let mut tasks = JoinSet::new();

    // Bridge DSSH control stream ↔ control stream socketpair.
    let bridge_shutdown = session_shutdown.clone();
    tasks.spawn(
        async move {
            tokio::select! {
                () = bridge_shutdown.cancelled() => {}
                _ = dssh::conversation::ipc::bridge_reader_to_unix(control_reader, ctrl_write) => {}
            }
        }
        .in_current_span(),
    );
    let bridge_shutdown = session_shutdown.clone();
    tasks.spawn(
        async move {
            tokio::select! {
                () = bridge_shutdown.cancelled() => {}
                _ = dssh::conversation::ipc::bridge_unix_to_writer(ctrl_read, control_writer) => {}
            }
        }
        .in_current_span(),
    );

    // Set up manage-stream RPC via IPC FD passing.
    let adapter = IpcManageStreamAdapter::new(manage_stream, fd_transfer.clone());
    let (ms, mc) = IpcManageSessionStreamServerShared::new(Arc::new(adapter), 8);
    let manage_shutdown = session_shutdown.clone();
    tasks.spawn(
        async move {
            tokio::select! {
                () = manage_shutdown.cancelled() => {}
                _ = ms.serve(true) => {}
            }
        }
        .in_current_span(),
    );

    let bootstrap = SessionBootstrap {
        manage_stream: mc,
        conversation_id,
        peer_version,
    };

    tracing::info!(%conversation_id, "calling StartSessionFn in child");
    let session_call = authenticated.start_session.call(bootstrap);
    tokio::pin!(session_call);

    tokio::select! {
        () = token.cancelled() => return CancelledSnafu.fail(),
        result = &mut conn => {
            result.context(RemocConnectionSnafu)?;
            return RemocClosedSnafu.fail();
        }
        result = &mut delivery => {
            let _delivered = result.context(DeliverControlFdSnafu)?;
        }
        result = &mut session_call => return result.context(SessionFailedSnafu),
    }

    let session_result = tokio::select! {
        () = token.cancelled() => CancelledSnafu.fail(),
        result = &mut conn => {
            result.context(RemocConnectionSnafu)?;
            RemocClosedSnafu.fail()
        }
        result = &mut session_call => result.context(SessionFailedSnafu),
    };

    // Session is done — tear down the remoc connection so the child sees
    // transport EOF on the socketpair and exits cleanly. Without this the
    // child's remoc connection future would hang forever and the
    // `pishoo-ssh-session` process would linger after the SSH session ends.
    session_shutdown.cancel();
    drop(tx);
    drop(rx);
    drop(conn);

    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            tracing::warn!(
                error = %Report::from_error(&error),
                "ssh session helper task failed"
            );
        }
    }

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
