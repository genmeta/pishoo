use std::{
    borrow::Cow,
    sync::{Arc, Mutex},
};

use axum::{Extension, extract::State, response::IntoResponse};
use dhttp::h3x::{connection::ConnectionState, qpack::field::Protocol, quic, stream_id::StreamId};
use dshell::{
    auth::AuthCredential,
    session::{AuthRequest, AuthenticateFn, AuthenticatedSession, SessionBootstrap},
};
use http::{Request, StatusCode};
use remoc::prelude::ServerShared;
use snafu::{OptionExt, Report, ResultExt, Snafu};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::{
    control_plane::DynSpawnSession,
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
        source: dhttp::h3x::ipc::transport::SplitError,
    },
    #[snafu(display("failed to establish remoc channel with child"))]
    RemocConnect {
        source: remoc::ConnectError<
            dhttp::h3x::ipc::transport::MuxSinkError,
            dhttp::h3x::ipc::transport::MuxStreamError,
        >,
    },
    #[snafu(display("remoc channel with child terminated"))]
    RemocConnection {
        source: remoc::chmux::ChMuxError<
            dhttp::h3x::ipc::transport::MuxSinkError,
            dhttp::h3x::ipc::transport::MuxStreamError,
        >,
    },
    #[snafu(display("remoc channel with child closed"))]
    RemocClosed,
    #[snafu(display("failed to receive AuthenticateFn from child"))]
    RecvAuthFn { source: remoc::rch::base::RecvError },
    #[snafu(display("child did not send AuthenticateFn"))]
    NoAuthFn,
    #[snafu(display("authentication rejected by child"))]
    AuthRejected { source: dshell::session::AuthError },
    #[snafu(display("child session failed"))]
    SessionFailed {
        source: dshell::session::SessionRunError,
    },
}

fn is_webtransport_request<B>(request: &Request<B>) -> bool {
    request
        .extensions()
        .get::<Protocol>()
        .is_some_and(|protocol| protocol.as_str() == dhttp::h3x::webtransport::WEBTRANSPORT_H3)
}

fn accept_server_session_error_status(
    error: &dshell::webtransport::AcceptServerSessionError,
) -> StatusCode {
    match error {
        dshell::webtransport::AcceptServerSessionError::UnexpectedPath { .. }
        | dshell::webtransport::AcceptServerSessionError::PeerVersion { .. }
        | dshell::webtransport::AcceptServerSessionError::Accept {
            source: dhttp::h3x::hyper::extended_connect::AcceptError::NotConnect { .. },
        } => StatusCode::BAD_REQUEST,
        dshell::webtransport::AcceptServerSessionError::Accept { .. }
        | dshell::webtransport::AcceptServerSessionError::RegisterSession { .. } => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Axum-style handler for DShell WebTransport CONNECT sessions.
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
        .ssh_deny()
        .map(|value| value.0.clone())
        .unwrap_or_default();

    if ssh_deny.iter().any(|d| d == username) {
        tracing::warn!(%username, "user denied by ssh_deny");
        return StatusCode::FORBIDDEN.into_response();
    }

    let conversation_id = stream_id;
    let username = username.to_owned();

    if !is_webtransport_request(&req) {
        tracing::warn!("dshell request is not webtransport extended connect");
        return StatusCode::BAD_REQUEST.into_response();
    }

    let Some(connection) = req
        .extensions()
        .get::<Arc<ConnectionState<dyn quic::DynConnection>>>()
        .cloned()
    else {
        tracing::warn!("dshell request is missing h3 connection state");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    if let Err(error) = connection.peer_settings().await {
        tracing::warn!(
            error = %Report::from_error(&error),
            "failed to wait for peer HTTP/3 settings before dshell webtransport accept"
        );
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let path = req.uri().path().to_owned();
    let accepted = match dshell::webtransport::accept_server_session(req, &path).await {
        Ok(accepted) => accepted,
        Err(error) => {
            let status = accept_server_session_error_status(&error);
            tracing::warn!(
                error = %Report::from_error(&error),
                "failed to accept dshell webtransport session"
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
            if let Err(error) = run_ssh_session(
                &username,
                conversation_id,
                AcceptedSshSession {
                    peer_version: accepted.peer_version,
                    session: accepted.session,
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

struct AcceptedSshSession {
    peer_version: String,
    session: dhttp::h3x::webtransport::WebTransportSession,
}

#[derive(Debug)]
struct SessionIpcLifecycle {
    token: CancellationToken,
    error: Mutex<Option<dhttp::h3x::quic::ConnectionError>>,
}

impl SessionIpcLifecycle {
    fn new(token: CancellationToken) -> Self {
        Self {
            token,
            error: Mutex::new(None),
        }
    }

    fn closed_error(&self) -> dhttp::h3x::quic::ConnectionError {
        let mut guard = self
            .error
            .lock()
            .expect("session ipc lifecycle lock poisoned");
        guard
            .get_or_insert_with(|| dhttp::h3x::quic::ConnectionError::Application {
                source: dhttp::h3x::quic::ApplicationError {
                    code: dhttp::h3x::error::Code::H3_REQUEST_CANCELLED,
                    reason: Cow::Borrowed("ssh session ipc closed"),
                },
            })
            .clone()
    }
}

impl dhttp::h3x::quic::Lifecycle for SessionIpcLifecycle {
    fn close(&self, code: dhttp::h3x::error::Code, reason: Cow<'static, str>) {
        let mut guard = self
            .error
            .lock()
            .expect("session ipc lifecycle lock poisoned");
        if guard.is_none() {
            *guard = Some(dhttp::h3x::quic::ConnectionError::Application {
                source: dhttp::h3x::quic::ApplicationError { code, reason },
            });
        }
        self.token.cancel();
    }

    fn check(&self) -> Result<(), dhttp::h3x::quic::ConnectionError> {
        if self.token.is_cancelled() {
            Err(self.closed_error())
        } else {
            Ok(())
        }
    }

    async fn closed(&self) -> dhttp::h3x::quic::ConnectionError {
        self.token.cancelled().await;
        self.closed_error()
    }
}

async fn run_ssh_session(
    username: &str,
    conversation_id: StreamId,
    accepted: AcceptedSshSession,
    spawner: &dyn DynSpawnSession,
    token: CancellationToken,
) -> Result<(), RunSshSessionError> {
    use run_ssh_session_error::*;

    let AcceptedSshSession {
        peer_version,
        session,
    } = accepted;

    // Spawn the session child process via the control plane.
    let transport = tokio::select! {
        () = token.cancelled() => return CancelledSnafu.fail(),
        result = spawner.spawn_session(username) => result.context(SpawnSessionSnafu)?,
    };

    // Establish remoc channel over MuxChannel with the session child.
    let mux = dhttp::h3x::ipc::transport::MuxChannel::from_fd(transport.mux_fd)
        .context(MuxChannelSnafu)?;
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

    let session_shutdown = token.child_token();
    let lifecycle: Arc<dyn dhttp::h3x::quic::DynLifecycle> =
        Arc::new(SessionIpcLifecycle::new(session_shutdown.clone()));
    let session = Arc::new(session);
    let session_id = dhttp::h3x::webtransport::Session::id(session.as_ref());

    let adapter = dhttp::h3x::ipc::webtransport::WebTransportSessionAdapter::new(
        Arc::clone(&session),
        fd_transfer.clone(),
        Arc::clone(&lifecycle),
    );
    let (webtransport_server, webtransport_client) =
        dhttp::h3x::ipc::webtransport::IpcWebTransportSessionServerShared::new(
            Arc::new(adapter),
            8,
        );

    let mut tasks = JoinSet::new();
    let webtransport_shutdown = session_shutdown.clone();
    tasks.spawn(
        async move {
            tokio::select! {
                () = webtransport_shutdown.cancelled() => {}
                _ = webtransport_server.serve(true) => {}
            }
        }
        .in_current_span(),
    );

    let bootstrap = SessionBootstrap {
        webtransport_session: dhttp::h3x::ipc::webtransport::WebTransportSessionBootstrap {
            session_id,
            session: webtransport_client,
        },
        peer_version,
    };

    tracing::info!(%conversation_id, "calling StartSessionFn in child");
    let session_call = authenticated.start_session.call(bootstrap);
    tokio::pin!(session_call);

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
    use dhttp::h3x::qpack::field::Protocol;

    use super::*;

    #[test]
    fn webtransport_connect_request_is_detected_by_protocol_extension() {
        let mut request = Request::builder().body(()).expect("request should build");
        request
            .extensions_mut()
            .insert(Protocol::new(dhttp::h3x::webtransport::WEBTRANSPORT_H3));

        assert!(is_webtransport_request(&request));
    }

    #[test]
    fn plain_connect_request_is_not_dshell_transport() {
        let request = Request::builder().body(()).expect("request should build");

        assert!(!is_webtransport_request(&request));
    }
}
