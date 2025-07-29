use futures::{SinkExt, TryStreamExt, never::Never};
use nix::unistd;
use ssh3_proto::messages::auth::{ClientAuthMessage, ServerAuthMessage};
use tokio::io;

use super::{
    Error,
    mux::{FramedRecver, FramedSender},
};
use crate::parse::{Node, Value};

pub async fn auth(
    username: &str,
    localhost: &str,
    location: &Node,
    mut sender: FramedSender<ServerAuthMessage>,
    recver: FramedRecver<ClientAuthMessage>,
) -> Result<unistd::User, Error> {
    reject_deny(username, location, &mut sender).await?;

    let user = match unistd::User::from_name(username) {
        Ok(Some(user)) => {
            tracing::debug!(target: "sshd", ?user, "User found");
            user
        }
        Ok(None) | Err(_) => {
            let reason = format!("User {username} not found");
            sender
                .cancel(io::Error::new(io::ErrorKind::NotFound, "User not found"))
                .await?;
            return Err(reason.into());
        }
    };

    let Some(Value::String(ssh_login)) = location.get("ssh_login") else {
        unreachable!();
    };
    match &**ssh_login {
        "basic" => auth_password(username, localhost, sender, recver).await?,
        "ssl" => _ = auth_ssl(sender).await?,
        _ => unreachable!("Unknown ssh_login type {ssh_login}"),
    }
    Ok(user)
}

pub async fn reject_deny(
    username: &str,
    location: &Node,
    sender: &mut FramedSender<ServerAuthMessage>,
) -> Result<(), Error> {
    if let Some(Value::StringVec(ssh_deny)) = location.get("ssh_deny")
        && ssh_deny.iter().any(|deny| &**deny == username)
    {
        sender
            .cancel(io::Error::new(io::ErrorKind::NotFound, "User not found"))
            .await?;
        return Err(format!("User {username} not allowed").into());
    }
    Ok(())
}

pub async fn auth_password(
    username: &str,
    localhost: &str,
    mut sender: FramedSender<ServerAuthMessage>,
    mut recver: FramedRecver<ClientAuthMessage>,
) -> Result<(), Error> {
    sender
        .send(ServerAuthMessage::Password {
            prompt: format!("{username}@{localhost}'s password: "),
        })
        .await?;

    let auth_password = async {
        const MAX_RETRIES: usize = 3;
        for i in 0..MAX_RETRIES {
            tracing::debug!(target: "sshd", times=i, "Waiting for password from client");
            let Some(message) = recver.try_next().await? else {
                return Err(Error::from("Failed to receive password from client"));
            };
            match message {
                ClientAuthMessage::Password(password) => {
                    let verify = tokio::task::spawn_blocking({
                        let username = username.to_owned();
                        move || verify_password(&username, &password)
                    });
                    match verify.await.unwrap() {
                        true => return Ok(true),
                        false if i == MAX_RETRIES - 1 => {
                            return Ok(false);
                        }
                        false => {
                            sender
                                .send(ServerAuthMessage::Password {
                                    prompt: format!(
                                        "Authentication failed, try again!\n{username}@{localhost}'s password: "
                                    ),
                                })
                                .await?;
                        }
                    }
                }
            }
        }

        Ok(false)
    };

    if auth_password.await? {
        sender.send(ServerAuthMessage::Accept).await?;
    } else {
        let reason = format!("Authentication failed for user {username}, too many retries.");
        sender
            .cancel(io::Error::new(
                io::ErrorKind::PermissionDenied,
                reason.clone(),
            ))
            .await?;
        return Err(reason.into());
    }
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

async fn auth_ssl(mut sender: FramedSender<ServerAuthMessage>) -> Result<Never, Error> {
    sender
        .cancel(io::Error::other(
            "SSL authentication not implemented in this version of pishoo",
        ))
        .await?;

    Err(Error::from("auth_ssl not implemented"))
}
