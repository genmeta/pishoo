//! pishoo-ssh-session: privilege-separated SSH3 session child process.
//!
//! Spawned by the gateway (pishoo) for each SSH3 connection.
//! Communicates with the parent via a remoc channel over stdin/stdout.
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
        AuthError, AuthRequest, SessionBootstrap, SessionRunError, StartSessionFn,
        dispatcher::{SessionConfig, run_session},
        privilege::drop_privileges,
    },
};
use snafu::Report;
use tracing::Instrument;

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

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    // Establish remoc channel over stdin/stdout.
    let (conn, mut tx, _rx) = remoc::Connect::io::<
        _,
        _,
        genmeta_ssh::session::AuthenticateFn,
        (),
        remoc::codec::Default,
    >(remoc::Cfg::default(), stdin, stdout)
    .await
    .expect("failed to establish remoc channel");
    let conn_handle = tokio::spawn(conn.instrument(tracing::info_span!("remoc_conn")));

    // Create the outer RFnOnce: authentication.
    let auth_fn = remoc::rfn::RFnOnce::new_1(|auth_request: AuthRequest| async move {
        tracing::info!(username = %auth_request.username, credential = %auth_request.credential, "authentication starting");

        let (uid, gid, shell) = match &auth_request.credential {
            #[cfg(feature = "pam")]
            AuthCredential::Basic { password, .. } => {
                // Password-based: full PAM authenticate + acct_mgmt.
                let user_info = genmeta_ssh::session::pam::authenticate(
                    "sshd",
                    &auth_request.username,
                    password,
                )
                .await
                .map_err(|e| AuthError::PamFailed {
                    reason: Report::from_error(e).to_string(),
                })?;
                (user_info.uid, user_info.gid, user_info.shell)
            }
            #[cfg(not(feature = "pam"))]
            AuthCredential::Basic { .. } => {
                return Err(AuthError::PamFailed {
                    reason: "password authentication requires the `pam` feature".to_owned(),
                });
            }
            #[cfg(feature = "pam")]
            AuthCredential::Certificate => {
                // mTLS: skip password authentication, but still perform
                // PAM acct_mgmt + open_session for system session creation.
                let user_info =
                    genmeta_ssh::session::pam::open_session("sshd", &auth_request.username)
                        .await
                        .map_err(|e| AuthError::PamFailed {
                            reason: Report::from_error(e).to_string(),
                        })?;
                (user_info.uid, user_info.gid, user_info.shell)
            }
            #[cfg(not(feature = "pam"))]
            AuthCredential::Certificate => {
                // mTLS without PAM: look up user directly from /etc/passwd.
                let user_info = genmeta_ssh::session::lookup_user(&auth_request.username)
                    .await
                    .map_err(|e| AuthError::PamFailed {
                        reason: Report::from_error(e).to_string(),
                    })?;
                (user_info.uid, user_info.gid, user_info.shell)
            }
        };

        tracing::info!(uid, gid, "authentication succeeded");

        let username = auth_request.username;

        // Create the inner RFnOnce: drop privileges + run session.
        let start_session_fn: StartSessionFn =
            remoc::rfn::RFnOnce::new_1(move |bootstrap: SessionBootstrap| async move {
                tracing::info!(%username, "starting session");

                if nix::unistd::getuid().is_root() {
                    drop_privileges(uid, gid, &username).map_err(|e| {
                        SessionRunError::DropPrivileges {
                            reason: Report::from_error(e).to_string(),
                        }
                    })?;
                    tracing::info!(uid, gid, "privileges dropped");
                }

                let control_reader = bootstrap.control_reader.into_box_reader();
                let control_writer = bootstrap.control_writer.into_box_writer();

                let conversation = Arc::new(Conversation::new(
                    bootstrap.conversation_id,
                    bootstrap.peer_version,
                    control_reader,
                    control_writer,
                    bootstrap.manage_stream,
                ));

                let config = SessionConfig {
                    shell,
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
