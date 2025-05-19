use futures::{SinkExt, TryStreamExt, never::Never};
use nix::unistd;
use serde::{Deserialize, Serialize};

use super::{
    Error,
    mux::{Recver, Sender},
};
use crate::parse::{Node, Value};

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientAuthMessage {
    Password(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerAuthMessage {
    Accpet,
    Password { prompt: String },
    Reject { reason: String },
}

pub async fn auth(
    username: &str,
    location: &Node,
    mut sender: Sender<ServerAuthMessage>,
    recver: Recver<ClientAuthMessage>,
) -> Result<unistd::User, Error> {
    reject_deny(username, location, &mut sender).await?;

    let user = match unistd::User::from_name(username) {
        Ok(Some(user)) => user,
        Ok(None) | Err(_) => {
            let reason = format!("User {username} not found");
            sender
                .send(ServerAuthMessage::Reject {
                    reason: reason.clone(),
                })
                .await?;
            return Err(reason.into());
        }
    };

    let Some(Value::String(ssh_login)) = location.get("ssh_login") else {
        unreachable!();
    };
    match &**ssh_login {
        "basic" => auth_password(username, sender, recver).await?,
        "ssl" => _ = auth_ssl(sender).await?,
        _ => unreachable!("Unknown ssh_login type {ssh_login}"),
    }
    Ok(user)
}

pub async fn reject_deny(
    username: &str,
    location: &Node,
    sender: &mut Sender<ServerAuthMessage>,
) -> Result<(), Error> {
    if let Some(Value::StringVec(ssh_deny)) = location.get("ssh_deny")
        && ssh_deny.iter().any(|deny| &**deny == username)
    {
        sender
            .send(ServerAuthMessage::Reject {
                reason: format!("User {username} not found"),
            })
            .await?;
        return Err(format!("User {username} not allowed").into());
    }
    Ok(())
}

pub async fn auth_password(
    username: &str,
    mut sender: Sender<ServerAuthMessage>,
    mut recver: Recver<ClientAuthMessage>,
) -> Result<(), Error> {
    sender
        .send(ServerAuthMessage::Password {
            prompt: format!("Please input password for {username}: "),
        })
        .await?;

    let auth_password = async {
        const MAX_RETRIES: usize = 3;
        for i in 0..MAX_RETRIES {
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
                                prompt: format!("Authentication failed, try again!\nPlease input password for {username}: "),
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
        sender.send(ServerAuthMessage::Accpet).await?;
    } else {
        let reason = format!("Authentication failed for user {username}");
        sender
            .send(ServerAuthMessage::Reject {
                reason: reason.clone(),
            })
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

async fn auth_ssl(mut sender: Sender<ServerAuthMessage>) -> Result<Never, Error> {
    sender
        .send(ServerAuthMessage::Reject {
            reason: "Server internal errror".to_owned(),
        })
        .await?;

    Err(Error::from("auth_ssl not implemented"))
}
