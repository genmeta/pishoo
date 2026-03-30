use std::{path::PathBuf, process::Stdio, sync::Arc};

use genmeta_ssh::{
    auth::AuthCredential,
    constants::SSH_VERSION,
    conversation::remoc::{ManageStreamBridge, RemoteManageStreamServerShared},
    session::{AuthRequest, AuthenticateFn, SessionBootstrap},
};
use h3x::{
    message::stream::{ReadStream, WriteStream},
    protocol::Protocols,
    quic::GetStreamIdExt,
    remoc::message::{ReadMessageStreamServer, WriteMessageStreamServer},
    stream_id::StreamId,
};
use http::{Request, StatusCode};
use remoc::prelude::{Server, ServerShared};
use snafu::{OptionExt, Report, ResultExt};
use tracing::Instrument;

use crate::{
    error::{Result, StreamSnafu, Whatever},
    parse::Node,
    reverse::log::RequestInfo,
};

/// Resolve the path of the `pishoo-ssh-session` binary.
///
/// - If `PISHOO_SSH_SESSION_BIN` env var was set **at compile time**, use it.
/// - Otherwise in debug builds, fall back to `<current_exe_dir>/pishoo-ssh-session`.
/// - In release builds without the env var, this is a compile error.
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

/// Handle an SSH3 session on an already-matched location.
///
/// The gateway uses mTLS (via the access/firewall subsystem) for client
/// authentication, so no username/password credential is extracted from the
/// HTTP Authorization header. Instead, the target username is taken from the
/// request path: if the SSH location is `/ssh/`, a request to `/ssh/yiyue`
/// means the user wants to log in as `yiyue`.
pub async fn serve(
    location: &Arc<Node>,
    final_pattern: &str,
    request: Request<()>,
    protocols: Arc<Protocols>,
    recver: ReadStream,
    sender: WriteStream,
) -> Result<()> {
    let req_info = RequestInfo::from_request(&request);

    let ssh_deny = location
        .get("ssh_deny")
        .map(|v| {
            let crate::parse::Value::StringVec(vec) = v else {
                unreachable!()
            };
            vec.to_owned()
        })
        .unwrap_or_default();

    let result =
        handle_ssh3_connect(final_pattern, request, protocols, ssh_deny, recver, sender).await;

    match &result {
        Ok(()) => {
            req_info.log_access(200, 0).await;
        }
        Err(e) => {
            req_info.log_error(Report::from_error(e)).await;
            req_info.log_access(500, 0).await;
        }
    }

    result
}

async fn send_status_and_close(mut sender: WriteStream, status: StatusCode) -> Result<()> {
    let response = http::Response::builder().status(status).body(()).unwrap();
    let (parts, _) = response.into_parts();
    sender
        .send_hyper_response_parts(parts)
        .await
        .context(StreamSnafu)?;
    sender.close().await.context(StreamSnafu)
}

async fn handle_ssh3_connect(
    final_pattern: &str,
    request: Request<()>,
    protocols: Arc<Protocols>,
    ssh_deny: Vec<String>,
    mut recver: ReadStream,
    mut sender: WriteStream,
) -> Result<()> {
    // Extract username from the request path.
    // The location pattern (e.g. "/ssh/") has been matched and `final_pattern`
    // contains the remaining suffix. For path "/ssh/yiyue" with location "/ssh/",
    // `final_pattern` is "yiyue".
    let username = final_pattern.trim_matches('/');
    if username.is_empty() {
        tracing::warn!("missing username in SSH path");
        send_status_and_close(sender, StatusCode::BAD_REQUEST).await?;
        return Ok(());
    }

    // Check denied users.
    if ssh_deny.iter().any(|d| d == username) {
        tracing::warn!(%username, "user denied by ssh_deny");
        send_status_and_close(sender, StatusCode::FORBIDDEN).await?;
        return Ok(());
    }

    // Validate ssh-version header.
    let peer_version = match request
        .headers()
        .get("ssh-version")
        .and_then(|v| v.to_str().ok())
    {
        Some(v) if v == SSH_VERSION => v.to_owned(),
        _ => {
            send_status_and_close(sender, StatusCode::BAD_REQUEST).await?;
            return Ok(());
        }
    };

    // Get StreamId from the underlying QUIC stream.
    let conversation_id = recver
        .stream_id()
        .await
        .whatever_context::<_, Whatever>("failed to get stream id")?;
    let conversation_id = StreamId::from(conversation_id);

    // Register conversation with the SSH3 protocol layer.
    let ssh3_proto = protocols
        .get::<genmeta_ssh::protocol::Ssh3Protocol>()
        .whatever_context::<_, Whatever>("Ssh3Protocol not registered")?;
    let handle = ssh3_proto
        .register(conversation_id)
        .whatever_context::<_, Whatever>("failed to register SSH3 conversation")?;

    // Spawn child process and perform PAM authentication.
    let session_binary = session_binary_path();

    let span = tracing::info_span!(
        "ssh-session",
        %conversation_id,
        user = %username
    );

    let mut child = match tokio::process::Command::new(&session_binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                error = %e,
                path = %session_binary.display(),
                "failed to spawn session binary"
            );
            send_status_and_close(sender, StatusCode::INTERNAL_SERVER_ERROR).await?;
            return Ok(());
        }
    };

    let child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();

    // Establish remoc channel: we receive AuthenticateFn from the child.
    let (conn, _tx, mut rx) =
        match remoc::Connect::io::<_, _, (), AuthenticateFn, remoc::codec::Default>(
            remoc::Cfg::default(),
            child_stdout,
            child_stdin,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to establish remoc channel with child");
                let _ = child.kill().await;
                send_status_and_close(sender, StatusCode::INTERNAL_SERVER_ERROR).await?;
                return Ok(());
            }
        };
    let conn_handle = tokio::spawn(conn.instrument(span.clone()));

    // Receive the AuthenticateFn from the child.
    let auth_fn: AuthenticateFn = match rx.recv().await {
        Ok(Some(f)) => f,
        _ => {
            tracing::error!("child did not send AuthenticateFn");
            let _ = child.kill().await;
            send_status_and_close(sender, StatusCode::INTERNAL_SERVER_ERROR).await?;
            return Ok(());
        }
    };

    // Call the child's PAM authentication.
    // mTLS-based: the client was already verified at the transport layer.
    let auth_request = AuthRequest {
        username: username.to_owned(),
        credential: AuthCredential::Certificate,
    };

    let start_session_fn = match auth_fn.call(auth_request).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %Report::from_error(&e), "authentication failed");
            let _ = child.kill().await;
            send_status_and_close(sender, StatusCode::UNAUTHORIZED).await?;
            return Ok(());
        }
    };

    // Auth succeeded — send 200 OK with ssh-version header.
    let response = http::Response::builder()
        .status(StatusCode::OK)
        .header("ssh-version", SSH_VERSION)
        .body(())
        .unwrap();
    let (parts, _) = response.into_parts();
    sender
        .send_hyper_response_parts(parts)
        .await
        .context(StreamSnafu)?;

    // Spawn session task: set up remoc bridges and call StartSessionFn.
    tokio::spawn(
        async move {
            // Serve control streams via remoc so the child can use them.
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

            // Serve the stream management bridge via remoc.
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
        }
        .instrument(span),
    );

    Ok(())
}
