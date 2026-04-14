use std::sync::Arc;

use axum::{Extension, extract::State, response::IntoResponse};
use genmeta_ssh::{
    auth::AuthCredential,
    constants::SSH_VERSION,
    conversation::remoc::{ManageStreamBridge, RemoteManageStreamServerShared},
    protocol::ConversationHandle,
    session::{AuthRequest, AuthenticateFn, SessionBootstrap},
};
use h3x::{
    hyper::upgrade,
    message::stream::{ReadStream, WriteStream},
    protocol::Protocols,
    remoc::message::{ReadMessageStreamServer, WriteMessageStreamServer},
    stream_id::StreamId,
};
use http::{Request, StatusCode};
use remoc::prelude::{Server, ServerShared};
use snafu::{OptionExt, Report, ResultExt, Snafu};
use tracing::Instrument;

use crate::{
    control_plane::DynSpawnSession,
    parse::Value,
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
    #[snafu(display("failed to establish remoc channel with child"))]
    RemocConnect {
        source: remoc::ConnectError<std::io::Error, std::io::Error>,
    },
    #[snafu(display("failed to receive AuthenticateFn from child"))]
    RecvAuthFn { source: remoc::rch::base::RecvError },
    #[snafu(display("child did not send AuthenticateFn"))]
    NoAuthFn,
    #[snafu(display("authentication rejected by child"))]
    AuthRejected {
        source: genmeta_ssh::session::AuthError,
    },
    #[snafu(display("child session failed"))]
    SessionFailed {
        source: genmeta_ssh::session::SessionRunError,
    },
}

/// Axum-style handler for SSH3 CONNECT sessions.
///
/// Extracts the username from `LocationMatch.remaining` (e.g. for `/ssh/yiyue`,
/// remaining is `"yiyue"`). Spawns the SSH session in a background task and
/// returns 200 OK with `ssh-version` header to complete the CONNECT upgrade.
pub async fn sshd_handle(
    Extension(loc): Extension<LocationMatch>,
    Extension(protocols): Extension<Arc<Protocols>>,
    Extension(stream_id): Extension<StreamId>,
    State(state): State<RouterState>,
    mut req: Request<axum::body::Body>,
) -> impl IntoResponse {
    let username = loc.remaining.trim_matches('/');
    if username.is_empty() {
        tracing::warn!("missing username in SSH path");
        return StatusCode::BAD_REQUEST.into_response();
    }

    let ssh_deny = loc
        .location
        .get("ssh_deny")
        .map(|v| {
            let Value::StringVec(vec) = v else {
                unreachable!()
            };
            vec.to_owned()
        })
        .unwrap_or_default();

    if ssh_deny.iter().any(|d| d == username) {
        tracing::warn!(%username, "user denied by ssh_deny");
        return StatusCode::FORBIDDEN.into_response();
    }

    let peer_version = match req
        .headers()
        .get("ssh-version")
        .and_then(|v| v.to_str().ok())
    {
        Some(v) if v == SSH_VERSION => v.to_owned(),
        _ => {
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    let conversation_id = stream_id;
    let username = username.to_owned();

    // Register the conversation BEFORE returning 200 OK. This ensures the
    // protocol layer can route incoming channel streams as soon as the client
    // receives the response and opens new QUIC bidi streams.
    let handle = match protocols.get::<genmeta_ssh::protocol::Ssh3Protocol>() {
        Some(proto) => match proto.register(conversation_id) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(error = %Report::from_error(&e), "failed to register SSH3 conversation");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        },
        None => {
            tracing::error!("ssh3 protocol not registered on connection");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let span = tracing::info_span!("ssh-session", %conversation_id, user = %username);

    // Spawn the SSH session in a background task. The CONNECT upgrade streams
    // become available after this handler returns the 200 response.
    tokio::spawn(
        async move {
            // Extract raw read/write streams via CONNECT upgrade.
            let read_stream = match upgrade::take::<ReadStream>(&mut req).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %Report::from_error(&e), "failed to take over read stream");
                    return;
                }
            };
            let write_stream = match upgrade::take::<WriteStream>(&mut req).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %Report::from_error(&e), "failed to take over write stream");
                    return;
                }
            };

            if let Err(e) = run_ssh_session(
                &username,
                conversation_id,
                peer_version,
                handle,
                state.session_spawner.as_ref(),
                read_stream,
                write_stream,
            )
            .await
            {
                tracing::error!(error = %Report::from_error(&e), "ssh session failed");
            }
        }
        .instrument(span),
    );

    // Return 200 OK with ssh-version header to accept the CONNECT.
    http::Response::builder()
        .status(StatusCode::OK)
        .header("ssh-version", SSH_VERSION)
        .body(axum::body::Body::empty())
        .unwrap()
        .into_response()
}

async fn run_ssh_session(
    username: &str,
    conversation_id: StreamId,
    peer_version: String,
    handle: ConversationHandle,
    spawner: &dyn DynSpawnSession,
    recver: ReadStream,
    sender: WriteStream,
) -> Result<(), RunSshSessionError> {
    use run_ssh_session_error::*;

    // Spawn the session child process via the control plane.
    let transport = spawner
        .spawn_session(username)
        .await
        .context(SpawnSessionSnafu)?;

    // Convert OwnedFd → tokio::fs::File for remoc IO.
    let stdin = tokio::fs::File::from_std(std::fs::File::from(transport.stdin));
    let stdout = tokio::fs::File::from_std(std::fs::File::from(transport.stdout));

    let (conn, _tx, mut rx) =
        remoc::Connect::io::<_, _, (), AuthenticateFn, remoc::codec::Default>(
            remoc::Cfg::default(),
            stdout,
            stdin,
        )
        .await
        .context(RemocConnectSnafu)?;
    let conn_handle = tokio::spawn(conn.in_current_span());

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

    // Set up remoc bridges for the control streams.
    let (rs, rc) = ReadMessageStreamServer::new(Box::pin(recver.into_bytes_stream()), 8);
    tokio::spawn(
        async move {
            let _ = rs.serve().await;
        }
        .in_current_span(),
    );

    let (ws, wc) = WriteMessageStreamServer::new(Box::pin(sender.into_bytes_sink()), 8);
    tokio::spawn(
        async move {
            let _ = ws.serve().await;
        }
        .in_current_span(),
    );

    let bridge = ManageStreamBridge::new(handle);
    let (ms, mc) = RemoteManageStreamServerShared::new(Arc::new(bridge), 8);
    tokio::spawn(
        async move {
            let _ = ms.serve(true).await;
        }
        .in_current_span(),
    );

    let bootstrap = SessionBootstrap {
        manage_stream: mc,
        control_reader: rc,
        control_writer: wc,
        conversation_id,
        peer_version,
    };

    tracing::info!(%conversation_id, "calling StartSessionFn in child");

    start_session_fn
        .call(bootstrap)
        .await
        .context(SessionFailedSnafu)?;

    let _ = conn_handle.await;
    tracing::info!(%conversation_id, "session ended");
    Ok(())
}
