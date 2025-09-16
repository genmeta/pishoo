//! WIP: error handling
use std::{self, sync::Arc};

use bytes::Bytes;
use firewall_base::pattern::{LocationPattern, LocationPatternKind, SUFFIX, trim_suffix_once};
use futures::{StreamExt, future::Either};
use h3::server::RequestStream;
use h3_shim::{RecvStream, SendStream};
use http::{Request, StatusCode};
use nix::unistd;
use snafu::{OptionExt, Report, ResultExt, whatever};
use ssh3_proto::{cbor_codec, messages, mux};
use tokio::{io, task::JoinSet};
use tokio_util::{codec, io::StreamReader};

use crate::{
    error::{Result, StreamSnafu},
    h3::{H3Sink, H3Stream},
    parse::Node,
    reverse::build_response,
};

mod async_fd;
mod auth;
mod forward;
mod session;
mod socks;

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
    rule_set: Option<&LocationPattern>,
    request: Request<()>,
    recver: RequestStream<RecvStream, Bytes>,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    if request.method() != http::Method::PUT {
        let resp = build_response(StatusCode::METHOD_NOT_ALLOWED);
        sender.send_response(resp).await.context(StreamSnafu)?;
        sender.finish().await.context(StreamSnafu)?;
        whatever!("Only PUT method is allowed");
    }

    let path = request.uri().path();
    if !path.starts_with(final_pattern) {
        let resp = build_response(StatusCode::BAD_REQUEST);
        sender.send_response(resp).await.context(StreamSnafu)?;
        sender.finish().await.context(StreamSnafu)?;
        whatever!("Request path {path} does not start with final pattern {final_pattern}");
    }

    let Some(crate::parse::Value::StringVec(auths)) = location.get("ssh_login") else {
        unreachable!()
    };

    assert!(!auths.is_empty(), "Checked in configuration parsing phase");

    let resp = build_response(StatusCode::OK);
    sender.send_response(resp).await.context(StreamSnafu)?;

    let localhost = request.uri().host().unwrap_or_default();
    // Server不是.genemta.net?
    let localhost = trim_suffix_once(localhost, SUFFIX).unwrap_or(localhost);

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
        let username = path[final_pattern.len()..].trim_start_matches('/');
        let user = auth(location, username, localhost, rule_set, &mut incomings).await?;

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
    rule_set: Option<&LocationPattern>,
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
            && rule_set.is_some_and(|pat| matches!(pat.kind(), LocationPatternKind::Exact))
            && path_username == username
        {
            auth::accept(&mut auth_sender).await?;
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
