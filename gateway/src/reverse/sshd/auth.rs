use futures::{SinkExt, TryStreamExt};
use nix::unistd;
use snafu::{OptionExt, ResultExt};
use ssh3_proto::messages::auth::{ClientAuthMessage, ServerAuthMessage};
use tokio::{io, task};

use super::mux::{FramedRecver, FramedSender};
use crate::parse::{Node, Value};

#[derive(snafu::Snafu, Debug)]
#[snafu(visibility(pub(in crate::reverse::sshd)))]
pub enum Error {
    #[snafu(display("User deny"))]
    Deny {},
    #[snafu(display("User not found"))]
    NotFound {},
    #[snafu(display("Cannot found user"))]
    CannotFoundUser { source: nix::errno::Errno },
    #[snafu(display("Too many attempts in password auth"))]
    TooManyAttempts {},
    // TODO: merge send error into channel error?
    #[snafu(display("Send message failed"))]
    Send { source: io::Error },
    #[snafu(display("Recv message failed"))]
    Recv { source: io::Error },
    #[snafu(display("Auth channel closed before auth completed"))]
    ChannelClosed {},
    #[snafu(display("Stream closed before received request"))]
    StreamClosed {},
    #[snafu(display("No suitable auth method found"))]
    NoMethod {},
}

pub async fn find_user(username: &str) -> Result<unistd::User, Error> {
    match unistd::User::from_name(username).context(CannotFoundUserSnafu)? {
        Some(user) => {
            tracing::debug!(target: "sshd", ?user, "User found");
            Ok(user)
        }
        None => NotFoundSnafu.fail(),
    }
}

pub async fn reject_deny(
    username: &str,
    location: &Node,
    sender: &mut FramedSender<ServerAuthMessage>,
) -> Result<(), Error> {
    if let Some(Value::StringVec(ssh_deny)) = location.get("ssh_deny")
        && ssh_deny.iter().any(|deny| &**deny == username)
    {
        _ = sender
            .cancel(io::Error::new(io::ErrorKind::NotFound, "User not found"))
            .await;
        return DenySnafu.fail();
    }
    Ok(())
}

pub async fn auth_password(
    username: &str,
    localhost: &str,
    sender: &mut FramedSender<ServerAuthMessage>,
    recver: &mut FramedRecver<ClientAuthMessage>,
) -> Result<(), Error> {
    let base_prompt = format!("{username}@{localhost}'s password: ");
    sender
        .send(ServerAuthMessage::Password {
            prompt: base_prompt.clone(),
        })
        .await
        .context(SendSnafu)?;

    let auth_password = async {
        const MAX_RETRIES: usize = 3;
        for i in 0..MAX_RETRIES {
            tracing::debug!(target: "auth", times=i, "Waiting for password from client");
            let message = recver
                .try_next()
                .await
                .context(RecvSnafu)?
                .context(ChannelClosedSnafu)?;
            match message {
                ClientAuthMessage::Password(password) => {
                    let verify = task::spawn_blocking({
                        let username = username.to_owned();
                        move || verify_password(&username, &password)
                    });
                    match verify.await.unwrap() {
                        true => return Ok(true),
                        false if i == MAX_RETRIES - 1 => {
                            return Ok(false);
                        }
                        false => sender
                            .send(ServerAuthMessage::Password {
                                prompt: format!("Authentication failed, try again!\n{base_prompt}"),
                            })
                            .await
                            .context(SendSnafu)?,
                    }
                }
            }
        }

        Ok(false)
    };

    if auth_password.await? {
        accept(sender).await?;
    } else {
        _ = sender
            .cancel(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("Authentication failed for user {username}, too many attempts."),
            ))
            .await;
        return TooManyAttemptsSnafu.fail();
    }
    Ok(())
}

pub async fn accept(sender: &mut FramedSender<ServerAuthMessage>) -> Result<(), Error> {
    sender
        .send(ServerAuthMessage::Accept)
        .await
        .context(SendSnafu)?;
    Ok(())
}

fn verify_password(username: &str, password: &str) -> bool {
    #[cfg(unix)]
    return {
        let mut auth = pam::Authenticator::with_password("login").expect("Init pam failed");
        auth.get_handler().set_credentials(username, password);
        auth.authenticate().is_ok()
    };

    #[allow(unreachable_code)]
    false
}
