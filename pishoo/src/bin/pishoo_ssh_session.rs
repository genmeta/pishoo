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
use tokio_util::{sync::CancellationToken, task::TaskTracker};
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

async fn run_authenticated_session(
    bootstrap: SessionBootstrap,
    user_info: UserInfo,
    username: String,
    fd_transfer: dhttp::h3x::ipc::transport::FdTransfer,
    shutdown: CancellationToken,
) -> Result<(), SessionRunError> {
    tracing::info!(%username, "starting session");

    if nix::unistd::getuid().is_root() {
        drop_privileges(user_info.uid, user_info.gid, &username).map_err(|error| {
            SessionRunError::DropPrivileges {
                reason: Report::from_error(error).to_string(),
            }
        })?;
        tracing::info!(
            uid = user_info.uid,
            gid = user_info.gid,
            "privileges dropped"
        );
    }

    let session_token = shutdown.child_token();
    let lifecycle: Arc<dyn dhttp::h3x::quic::DynLifecycle> =
        Arc::new(SessionIpcLifecycle::new(session_token.clone()));

    let result = async {
        let session = dhttp::h3x::ipc::webtransport::IpcWebTransportSessionHandle::new(
            bootstrap.webtransport_session.session_id,
            bootstrap.webtransport_session.session,
            fd_transfer,
            lifecycle,
        );

        let conversation = tokio::select! {
            () = session_token.cancelled() => return Ok(()),
            result = Conversation::accept(session, bootstrap.peer_version) => Arc::new(
                result.map_err(|error| SessionRunError::ConversationBuild {
                    reason: Report::from_error(&error).to_string(),
                })?
            ),
        };

        let config = SessionConfig {
            user: user_info,
            ..Default::default()
        };

        tracing::info!("session dispatcher starting");
        let outcome = run_session(conversation, config, session_token.clone())
            .await
            .map_err(|error| SessionRunError::Session {
                reason: Report::from_error(&error).to_string(),
            })?;
        tracing::info!(?outcome, "session ended");
        Ok(())
    }
    .await;

    session_token.cancel();
    result
}

// remoc runs remote functions with tokio::spawn. A current-thread runtime keeps
// PAM session setup on the process leader, as required by Linux pam_loginuid.
#[tokio::main(flavor = "current_thread")]
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
    let helper_shutdown = CancellationToken::new();
    let session_tasks = TaskTracker::new();

    // Create the outer RFnOnce: authentication.
    let auth_fd_transfer = fd_transfer.clone();
    let auth_shutdown = helper_shutdown.clone();
    let auth_session_tasks = session_tasks.clone();
    let auth_fn = remoc::rfn::RFnOnce::new_1(move |auth_request: AuthRequest| {
        let fd_transfer = auth_fd_transfer.clone();
        let helper_shutdown = auth_shutdown.clone();
        let session_tasks = auth_session_tasks.clone();
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
            let start_shutdown = helper_shutdown.clone();
            let start_session_tasks = session_tasks.clone();

            // Keep the session task alive even if remoc drops the RPC future.
            let start_session_fn: StartSessionFn =
                remoc::rfn::RFnOnce::new_1(move |bootstrap: SessionBootstrap| async move {
                    let task = start_session_tasks.spawn(run_authenticated_session(
                        bootstrap,
                        user_info,
                        username,
                        session_fd_transfer,
                        start_shutdown.child_token(),
                    ));
                    task.await.map_err(|error| SessionRunError::Session {
                        reason: format!("session task failed: {error}"),
                    })?
                });

            Ok(AuthenticatedSession {
                start_session: start_session_fn,
            })
        }
    });

    let auth_sent = tokio::select! {
        result = &mut conn => {
            if let Err(error) = result {
                tracing::warn!(
                    error = %Report::from_error(&error),
                    "remoc connection ended before AuthenticateFn was sent"
                );
            }
            false
        }
        result = tx.send(auth_fn) => {
            result.expect("failed to send AuthenticateFn to parent");
            true
        }
    };

    drop(tx);
    if auth_sent {
        let mut term_signal =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to register SIGTERM listener");
        let mut int_signal =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("failed to register SIGINT listener");

        let connection_result = tokio::select! {
            result = &mut conn => Some(result),
            _ = term_signal.recv() => {
                tracing::info!(signal = "SIGTERM", "received shutdown signal");
                None
            }
            _ = int_signal.recv() => {
                tracing::info!(signal = "SIGINT", "received shutdown signal");
                None
            }
        };
        if let Some(Err(error)) = connection_result {
            tracing::debug!(
                error = %Report::from_error(&error),
                "remoc connection ended"
            );
        }
    }

    drop(conn);
    helper_shutdown.cancel();
    session_tasks.close();
    session_tasks.wait().await;
    tracing::info!("ssh session process exiting");
}
