//! pishoo-ssh-session: privilege-separated SSH3 session child process.
//!
//! Spawned by the gateway (pishoo) for each SSH3 connection.
//! Communicates with the parent via a remoc channel over a MuxChannel
//! socketpair on FD 3.
//!
//! Flow:
//! 1. Send `AuthenticateFn` to parent over remoc
//! 2. Parent calls it with `AuthRequest` → child runs PAM authentication
//! 3. On success, return `StartSessionFn` to parent
//! 4. Parent calls it with `SessionBootstrap` → child drops privileges
//!    and runs the session dispatcher

use std::sync::Arc;

use genmeta_ssh::{
    auth::AuthCredential,
    conversation::Conversation,
    session::{
        AuthError, AuthRequest, SessionBootstrap, SessionRunError, StartSessionFn, UserInfo,
        dispatcher::{SessionConfig, run_session},
        privilege::drop_privileges,
    },
};
use h3x::ipc::transport::MuxChannel;
use snafu::Report;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;

#[tokio::main]
async fn main() {
    let user = std::env::var("PISHOO_USER").unwrap_or_else(|_| {
        eprintln!("PISHOO_USER not set; this binary must be spawned by pishoo");
        std::process::exit(1);
    });
    let _tracing_guard = pishoo::tracing_init::init_tracing(&format!(
        "sshd-session:{}/{}",
        user,
        std::process::id()
    ));

    // Recover the MuxChannel FD from FD 3 (dup2'd by root in session_child_exec).
    let mux_fd = {
        use std::os::fd::FromRawFd;
        // SAFETY: the root process dup2'd the socketpair FD to FD 3 in
        // session_child_exec before execve. FD 3 is guaranteed to be open.
        unsafe { std::os::fd::OwnedFd::from_raw_fd(3) }
    };

    let mux = MuxChannel::from_fd(mux_fd).expect("failed to create MuxChannel from fd 3");
    let (sink, stream) = mux.split().expect("failed to split MuxChannel");

    // Capture FD registry before remoc consumes the stream.
    let fd_registry = stream.fd_registry();

    // Establish remoc channel over MuxSink/MuxStream.
    let (conn, mut tx, _rx) = remoc::Connect::framed::<
        _,
        _,
        genmeta_ssh::session::AuthenticateFn,
        (),
        remoc::codec::Default,
    >(remoc::Cfg::default(), sink, stream)
    .await
    .expect("failed to establish remoc channel");
    let conn_handle = AbortOnDropHandle::new(tokio::spawn(
        conn.instrument(tracing::info_span!("remoc_conn")),
    ));

    // Create the outer RFnOnce: authentication.
    let auth_fn = remoc::rfn::RFnOnce::new_1(|auth_request: AuthRequest| async move {
        tracing::info!(username = %auth_request.username, credential = %auth_request.credential, "authentication starting");

        let user_info: UserInfo = match &auth_request.credential {
            AuthCredential::Basic { .. } => {
                return Err(AuthError::PamFailed {
                    reason: "password authentication is no longer supported".to_owned(),
                });
            }
            #[cfg(feature = "pam")]
            AuthCredential::Certificate => {
                // mTLS: skip password authentication, but still perform
                // PAM acct_mgmt + open_session for system session creation.
                genmeta_ssh::session::pam::open_session("sshd", &auth_request.username)
                    .await
                    .map_err(|e| AuthError::PamFailed {
                        reason: Report::from_error(e).to_string(),
                    })?
            }
            #[cfg(not(feature = "pam"))]
            AuthCredential::Certificate => {
                // mTLS without PAM: look up user directly from /etc/passwd.
                let user_info = genmeta_ssh::session::lookup_user(&auth_request.username)
                    .await
                    .map_err(|e| AuthError::PamFailed {
                        reason: Report::from_error(e).to_string(),
                    })?;
                // Without PAM, explicitly check /etc/nologin.
                if let Err(msg) = genmeta_ssh::session::check_nologin(user_info.uid) {
                    return Err(AuthError::PamFailed { reason: msg });
                }
                user_info
            }
        };

        tracing::info!(
            uid = user_info.uid,
            gid = user_info.gid,
            "authentication succeeded"
        );

        let username = auth_request.username;

        // Create the inner RFnOnce: drop privileges + run session.
        let start_session_fn: StartSessionFn =
            remoc::rfn::RFnOnce::new_1(move |bootstrap: SessionBootstrap| async move {
                tracing::info!(%username, "starting session");

                if nix::unistd::getuid().is_root() {
                    drop_privileges(user_info.uid, user_info.gid, &username).map_err(|e| {
                        SessionRunError::DropPrivileges {
                            reason: Report::from_error(e).to_string(),
                        }
                    })?;
                    tracing::info!(
                        uid = user_info.uid,
                        gid = user_info.gid,
                        "privileges dropped"
                    );
                }

                // Resolve control stream from FD registry.
                let fds = fd_registry
                    .wait_fds(bootstrap.control_fd_id)
                    .await
                    .map_err(|e| SessionRunError::ConversationBuild {
                        reason: Report::from_error(e).to_string(),
                    })?;
                let ctrl_fd =
                    fds.into_iter()
                        .next()
                        .ok_or_else(|| SessionRunError::ConversationBuild {
                            reason: "expected 1 FD for control stream, got 0".into(),
                        })?;
                let ctrl_unix = {
                    let ctrl_std = std::os::unix::net::UnixStream::from(ctrl_fd);
                    ctrl_std.set_nonblocking(true).map_err(|e| {
                        SessionRunError::ConversationBuild {
                            reason: format!("failed to set control FD nonblocking: {e}"),
                        }
                    })?;
                    tokio::net::UnixStream::from_std(ctrl_std).map_err(|e| {
                        SessionRunError::ConversationBuild {
                            reason: format!("failed to convert control FD to tokio stream: {e}"),
                        }
                    })?
                };
                let (control_reader, control_writer) = ctrl_unix.into_split();

                // Create IPC manage stream handle.
                let manage_stream = genmeta_ssh::conversation::ipc::IpcManageStreamHandle::new(
                    bootstrap.manage_stream,
                    fd_registry,
                );

                let conversation = Arc::new(Conversation::new(
                    bootstrap.conversation_id,
                    bootstrap.peer_version,
                    control_reader,
                    control_writer,
                    manage_stream,
                ));

                let config = SessionConfig {
                    user: user_info,
                    ..Default::default()
                };

                tracing::info!("session dispatcher starting");
                run_session(conversation, config).await;
                tracing::info!("session ended");
                Ok(())
            });

        Ok(start_session_fn)
    });

    tx.send(auth_fn)
        .await
        .expect("failed to send AuthenticateFn to parent");

    let _ = conn_handle.await;
    tracing::info!("ssh session process exiting");
}
