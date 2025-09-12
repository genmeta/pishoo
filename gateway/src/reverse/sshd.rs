//! WIP: error handling
use std::{self, sync::Arc};

use bytes::Bytes;
use firewall_base::pattern::{LocationPattern, LocationPatternKind};
use futures::{StreamExt, future::Either};
use h3::server::RequestStream;
use h3_shim::{RecvStream, SendStream};
use http::{Request, StatusCode};
use nix::unistd;
use snafu::{OptionExt, Report, ResultExt, ensure_whatever, whatever};
use ssh3_proto::{cbor_codec, messages, mux};
use tokio::{io, task::JoinSet};
use tokio_util::{codec, io::StreamReader};

use crate::error::Whatever;

mod async_fd;
mod auth;
mod forward;
mod session;
mod socks;
use crate::{
    h3::{H3Sink, H3Stream},
    parse::Node,
};

/// ``` conf
/// location /ssh {
///     ssh_login basic ssl; # ssl 需要结合防火墙使用
///     ssh_deny root;
/// }
/// ```
///
/// 配置精确locations匹配，既可免密登录
/// ``` shell
/// access domain "ssh.api.server" "= /ssh/ubuntu" allow "*.admin.api.server"
/// ```
pub async fn login(
    location: &Arc<Node>,
    final_pattern: &str,
    firewall_matched_location: Option<&LocationPattern>,
    request: Request<()>,
    recver: RequestStream<RecvStream, Bytes>,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<(), Whatever> {
    let mut response_with_status = async |status: StatusCode| {
        let resp = http::Response::builder().status(status).body(()).unwrap();
        sender.send_response(resp).await?;
        sender.finish().await
    };

    if request.method() != http::Method::PUT {
        response_with_status(StatusCode::METHOD_NOT_ALLOWED)
            .await
            .whatever_context("http error")?;
        whatever!("Only PUT method is allowed");
    }

    let Some(crate::parse::Value::StringVec(auths)) = location.get("ssh_login") else {
        unreachable!()
    };

    ensure_whatever!(!auths.is_empty(), "No auth method configured");

    let localhost = request.uri().host().unwrap_or_default();
    let localhost = localhost.strip_suffix(".genmeta.net").unwrap_or(localhost);
    let path = request.uri().path();
    ensure_whatever!(
        path.starts_with(final_pattern),
        "Request path {path} does not start with final pattern {final_pattern}, this should not happen"
    );

    let (mux, mut incomings) = mux::Mux::new(
        mux::Role::Server,
        codec::FramedRead::new(
            StreamReader::new(H3Stream::new(recver)),
            cbor_codec::CborDecoder::default(),
        ),
        codec::FramedWrite::new(
            H3Sink::new(sender), // No need for SinkWriter
            cbor_codec::CborEncoder::default(),
        ),
    );

    let run = async {
        let user = auth(
            location,
            path[final_pattern.len()..].trim_start_matches('/'),
            localhost,
            firewall_matched_location,
            &mut incomings,
        )
        .await?;

        serve(user, mux, incomings).await
    };

    if let Err(error) = run.await {
        tracing::error!(target: "sshd", "End with error: {}", Report::from_error(&error));
    }

    Ok(())
}

async fn auth(
    location: &Arc<Node>,
    path_username: &str,
    localhost: &str,
    firewall_matched_location: Option<&LocationPattern>,
    incomings: &mut mux::Incomings,
) -> Result<unistd::User, SshdError> {
    let Some(crate::parse::Value::StringVec(auths)) = location.get("ssh_login") else {
        unreachable!()
    };

    let auth_channel = incomings
        .next()
        .await
        .context(auth::StreamClosedSnafu)
        .context(AuthSnafu { username: None })?
        .context(ReceiveMessageSnafu)?;

    let messages::OpenChannel::Auth { username } = auth_channel.request else {
        return UnexpectedRequestSnafu {
            expect: "Auth",
            request: auth_channel.request,
        }
        .fail();
    };
    let mut auth_sender = auth_channel.sender.framed();
    let mut auth_recver = auth_channel.recver.framed();
    let user: unistd::User = async {
        auth::reject_deny(&username, location, &mut auth_sender).await?;

        let user = auth::find_user(&username).await?;

        if auths.iter().any(|auth| auth == "ssl")
            && firewall_matched_location
                .is_some_and(|pat| matches!(pat.kind(), LocationPatternKind::Exact))
            && path_username == username
        {
            return Ok(user);
        }

        if auths.iter().any(|auth| auth == "basic") {
            auth::auth_password(&username, localhost, &mut auth_sender, &mut auth_recver).await?;
            return Ok(user);
        }

        unreachable!("No suitable auth method found, but this should have been caught earlier");
    }
    .await
    .context(AuthSnafu { username })?;
    Ok(user)
}

#[derive(snafu::Snafu, Debug)]
enum SshdError {
    #[snafu(display("Auth for login `{}` failed", username.as_deref().unwrap_or("<unknown>")))]
    Auth {
        source: auth::Error,
        username: Option<String>,
    },
    #[snafu(display("An error occurred while processing the peer's request `{request}`"))]
    HandleRequest {
        request: messages::OpenChannel,
        #[snafu(source(from(Box<dyn snafu::Error + Send + Sync>, std::convert::identity)))]
        source: Box<dyn snafu::Error + Send + Sync>,
    },
    #[snafu(display("Unexpected request `{request}`, expect {expect}"))]
    UnexpectedRequest {
        expect: &'static str,
        request: messages::OpenChannel,
    },
    #[snafu(display("Failed to receive message"))]
    ReceiveMessage {
        source: mux::ForwardError<io::Error>,
    },
}

async fn serve(
    user: unistd::User,
    mux: Arc<mux::Mux>,
    mut incomings: mux::Incomings,
) -> Result<(), SshdError> {
    let mut tasks = JoinSet::new();
    let mut handle_new_channel =
        async |mux::NewChannel {
                   token,
                   request,
                   sender,
                   recver,
               }: mux::NewChannel| {
            match request.clone() {
                messages::OpenChannel::Auth { .. } => {
                    Err("Client send Auth request after  completed".into())
                        .context(HandleRequestSnafu { request })
                }
                messages::OpenChannel::Shell { pseudo } => {
                    let pseudo = pseudo.to_owned();
                    let shell = session::shell(&user, pseudo, recver, sender)
                        .await
                        .map_err(Into::into)
                        .context(HandleRequestSnafu { request })?;
                    tasks.spawn(shell);
                    Ok(())
                }
                messages::OpenChannel::Exec { pseudo, command } => {
                    let pseudo = pseudo.to_owned();
                    let command = command.to_owned();
                    let exec = session::exec(&user, pseudo, Some(&command), recver, sender)
                        .await
                        .map_err(Into::into)
                        .context(HandleRequestSnafu { request })?;
                    tasks.spawn(exec);
                    Ok(())
                }
                messages::OpenChannel::Direct { to: local } => {
                    let forward = forward::accept_forward(sender, recver, local.clone())
                        .await
                        .map_err(Into::into)
                        .context(HandleRequestSnafu { request })?;
                    tasks.spawn(async move {
                        if let Err(error) = forward.await {
                            tracing::error!(target: "local_forward", "Failed to forward data to `{local}`: {}", Report::from_error(&error));
                        }
                    });
                    Ok(())
                }
                messages::OpenChannel::Forward { listen, socks } => {
                    let mux = mux.clone();
                    let listen_result = if socks {
                        socks::listen_remote_forward(mux, token, sender, recver, listen.clone())
                            .await
                            .map(Either::Left)
                    } else {
                        forward::listen_remote_forward(mux, token, sender, recver, listen.clone())
                            .await
                            .map(Either::Right)
                    };

                    let accept_forward = listen_result
                        .map_err(Into::into)
                        .context(HandleRequestSnafu { request })?;

                    tasks.spawn(async move {
                    if let Err(accept_error) = accept_forward.await {
                        tracing::error!(target: "remote_forward", "Failed to accept incoming connection to `{listen}`: {}", Report::from_error(&accept_error));
                    }
                });
                    Ok(())
                }
                request => UnexpectedRequestSnafu {
                    expect: "Shell, Exec, Direct or Forward",
                    request,
                }
                .fail(),
            }
        };

    while let Some(new_channel) = incomings
        .next()
        .await
        .transpose()
        .context(ReceiveMessageSnafu)?
    {
        if let Err(handle_error) = handle_new_channel(new_channel).await {
            tracing::warn!(target: "sshd", "Error occurs in handling new channel, ending tasks: {}", Report::from_error(&handle_error));
            tasks.detach_all();
            return Err(handle_error);
        }
    }
    _ = tasks.join_all().await;

    Ok(())
}
