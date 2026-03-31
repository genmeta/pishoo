use std::{path::PathBuf, process::Stdio, sync::Arc};

use axum::{Extension, response::IntoResponse};
use genmeta_ssh::{
    auth::AuthCredential,
    constants::SSH_VERSION,
    conversation::remoc::{ManageStreamBridge, RemoteManageStreamServerShared},
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
use snafu::Report;
use tracing::Instrument;

use crate::{parse::Value, reverse::location::LocationMatch};

/// Resolve the path of the `pishoo-ssh-session` binary.
fn session_binary_path() -> PathBuf {
    #[allow(clippy::option_env_unwrap)]
    {
        #[cfg(debug_assertions)]
        {
            match option_env!("PISHOO_SSH_SESSION_BIN") {
                Some(path) => PathBuf::from(path),
                None => std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join("pishoo-ssh-session")))
                    .unwrap_or_else(|| PathBuf::from("pishoo-ssh-session")),
            }
        }
        #[cfg(not(debug_assertions))]
        {
            PathBuf::from(env!("PISHOO_SSH_SESSION_BIN"))
        }
    }
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

    let span = tracing::info_span!("ssh-session", %conversation_id, user = %username);

    // Spawn the SSH session in a background task. The CONNECT upgrade streams
    // become available after this handler returns the 200 response.
    tokio::spawn(async move {
        // Extract raw read/write streams via CONNECT upgrade.
        let read_stream = match upgrade::take::<ReadStream>(&mut req).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to take over read stream");
                return;
            }
        };
        let write_stream = match upgrade::take::<WriteStream>(&mut req).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to take over write stream");
                return;
            }
        };

        if let Err(e) = run_ssh_session(
            &username,
            conversation_id,
            peer_version,
            protocols,
            read_stream,
            write_stream,
        )
        .await
        {
            tracing::error!(error = %e, "SSH session failed");
        }
    }.instrument(span));

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
    protocols: Arc<Protocols>,
    recver: ReadStream,
    sender: WriteStream,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ssh3_proto = protocols
        .get::<genmeta_ssh::protocol::Ssh3Protocol>()
        .ok_or("Ssh3Protocol not registered")?;
    let handle = ssh3_proto
        .register(conversation_id)
        .map_err(|e| format!("failed to register SSH3 conversation: {e}"))?;

    let session_binary = session_binary_path();

    let mut child = tokio::process::Command::new(&session_binary)
        .env("PISHOO_USER", username)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            tracing::error!(
                error = %e,
                path = %session_binary.display(),
                "failed to spawn session binary"
            );
            e
        })?;

    let child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();

    let (conn, _tx, mut rx) =
        remoc::Connect::io::<_, _, (), AuthenticateFn, remoc::codec::Default>(
            remoc::Cfg::default(),
            child_stdout,
            child_stdin,
        )
        .await?;
    let conn_handle = tokio::spawn(conn.in_current_span());

    let auth_fn: AuthenticateFn = rx
        .recv()
        .await?
        .ok_or("child did not send AuthenticateFn")?;

    let auth_request = AuthRequest {
        username: username.to_owned(),
        credential: AuthCredential::Certificate,
    };

    let start_session_fn = auth_fn.call(auth_request).await.map_err(|e| {
        tracing::warn!(error = %Report::from_error(&e), "authentication failed");
        e
    })?;

    // Set up remoc bridges for the control streams.
    let (rs, rc) = ReadMessageStreamServer::new(Box::pin(recver.into_bytes_stream()), 1);
    tokio::spawn(
        async move {
            let _ = rs.serve().await;
        }
        .in_current_span(),
    );

    let (ws, wc) = WriteMessageStreamServer::new(Box::pin(sender.into_bytes_sink()), 1);
    tokio::spawn(
        async move {
            let _ = ws.serve().await;
        }
        .in_current_span(),
    );

    let bridge = ManageStreamBridge::new(handle);
    let (ms, mc) = RemoteManageStreamServerShared::new(Arc::new(bridge), 1);
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

    match start_session_fn.call(bootstrap).await {
        Ok(()) => tracing::info!(%conversation_id, "child session completed"),
        Err(e) => tracing::error!(
            error = %Report::from_error(&e),
            "child session failed"
        ),
    }

    let _ = child.wait().await;
    let _ = conn_handle.await;
    tracing::info!(%conversation_id, "session ended");
    Ok(())
}
