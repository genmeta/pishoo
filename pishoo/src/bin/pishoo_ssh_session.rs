//! pishoo-ssh-session: privilege-separated DShell session child process.
//!
//! Spawned by the gateway (pishoo) for each DShell connection.
//! Communicates with the parent via a remoc channel over a MuxChannel
//! socketpair on FD 3.
//!
//! Flow:
//! 1. Send `AuthenticateFn` to parent over remoc
//! 2. Parent calls it with `AuthRequest` → child runs PAM authentication
//! 3. On success, return `StartSessionFn` to parent
//! 4. Parent calls it with `SessionBootstrap` → child drops privileges
//!    and runs the session dispatcher

use std::{
    borrow::Cow,
    sync::{Arc, Mutex},
};

use dhttp::h3x::ipc::transport::MuxChannel;
use dshell::{
    auth::AuthCredential,
    conversation::Conversation,
    session::{
        AuthError, AuthRequest, AuthenticatedSession, SessionBootstrap, SessionRunError,
        StartSessionFn, UserInfo,
        dispatcher::{SessionConfig, run_session},
        privilege::drop_privileges,
    },
};
use snafu::Report;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

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

    // Capture the FD transfer plane before remoc consumes the transport.
    let fd_transfer = stream.fd_transfer(sink.fd_sender());

    // Establish remoc channel over MuxSink/MuxStream.
    let (conn, mut tx, _rx) =
        remoc::Connect::framed::<_, _, dshell::session::AuthenticateFn, (), remoc::codec::Default>(
            remoc::Cfg::default(),
            sink,
            stream,
        )
        .await
        .expect("failed to establish remoc channel");
    let mut conn = Box::pin(conn.instrument(tracing::info_span!("remoc_conn")));

    // Create the outer RFnOnce: authentication.
    let auth_fd_transfer = fd_transfer.clone();
    let auth_fn = remoc::rfn::RFnOnce::new_1(move |auth_request: AuthRequest| {
        let fd_transfer = auth_fd_transfer.clone();
        async move {
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
                    dshell::session::pam::open_session("sshd", &auth_request.username)
                        .await
                        .map_err(|e| AuthError::PamFailed {
                            reason: Report::from_error(e).to_string(),
                        })?
                }
                #[cfg(not(feature = "pam"))]
                AuthCredential::Certificate => {
                    // mTLS without PAM: look up user directly from /etc/passwd.
                    let user_info = dshell::session::lookup_user(&auth_request.username)
                        .await
                        .map_err(|e| AuthError::PamFailed {
                            reason: Report::from_error(e).to_string(),
                        })?;
                    // Without PAM, explicitly check /etc/nologin.
                    if let Err(msg) = dshell::session::check_nologin(user_info.uid) {
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
            let session_fd_transfer = fd_transfer.clone();

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

                    let session_token = CancellationToken::new();
                    let lifecycle: Arc<dyn dhttp::h3x::quic::DynLifecycle> =
                        Arc::new(SessionIpcLifecycle::new(session_token.clone()));
                    let session = dhttp::h3x::ipc::webtransport::IpcWebTransportSessionHandle::new(
                        bootstrap.webtransport_session.session_id,
                        bootstrap.webtransport_session.session,
                        session_fd_transfer,
                        lifecycle,
                    );

                    let conversation = Arc::new(
                        Conversation::accept(session, bootstrap.peer_version)
                            .await
                            .map_err(|e| SessionRunError::ConversationBuild {
                                reason: Report::from_error(&e).to_string(),
                            })?,
                    );

                    let config = SessionConfig {
                        user: user_info,
                        ..Default::default()
                    };

                    tracing::info!("session dispatcher starting");
                    let outcome = run_session(conversation, config).await.map_err(|e| {
                        SessionRunError::Session {
                            reason: Report::from_error(&e).to_string(),
                        }
                    })?;
                    session_token.cancel();
                    tracing::info!(?outcome, "session ended");
                    Ok(())
                });

            Ok(AuthenticatedSession {
                start_session: start_session_fn,
            })
        }
    });

    tokio::select! {
        result = &mut conn => {
            if let Err(error) = result {
                tracing::warn!(
                    error = %Report::from_error(&error),
                    "remoc connection ended before AuthenticateFn was sent"
                );
            }
            return;
        }
        result = tx.send(auth_fn) => {
            result.expect("failed to send AuthenticateFn to parent");
        }
    }

    drop(tx);
    if let Err(error) = conn.await {
        tracing::debug!(
            error = %Report::from_error(&error),
            "remoc connection ended"
        );
    }
    tracing::info!("ssh session process exiting");
}
