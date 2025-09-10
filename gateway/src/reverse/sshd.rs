//! WIP: error handling
use std::{self, sync::Arc};

use bytes::Bytes;
use futures::{StreamExt, future::Either};
use gm_quic::Connection;
use h3::server::RequestStream;
use h3_shim::{RecvStream, SendStream};
use http::{Request, Response, StatusCode};
use snafu::{OptionExt, ResultExt};
use ssh3_proto::{cbor_codec, messages, mux};
use tokio::{io, task::JoinSet};
use tokio_util::{codec, io::StreamReader};

mod async_fd;
mod auth;
mod forward;
mod session;
mod socks;
use crate::{
    h3::{H3Sink, H3Stream},
    parse::Node,
};

fn map_errno(errno: nix::Error, message: &str) -> io::Error {
    let error = io::Error::from(errno);
    io::Error::new(error.kind(), format!("{message}: {errno}"))
}

type Error = Box<dyn std::error::Error + Send + Sync>;

/// ``` conf
/// location /ssh {
///     ssh_login basic | ssl; # ssl 需要server级配置ssl_verify_client
///
///     # 如果是ssl证书认证，可能有多个证书/客户端名字，对应多个用户；
///     # 也可能是一个客户端名字，可以变换多个用户
///     ssh_ssl_user alice.genmeta.net alice; # ssl证书验证有效
///     ssh_ssl_user bob.genmeta.net bob;
///     ssh_ssl_user xxx.genmeta.net $user; # 很多用户都用同一个证书
///
///     # basic auth就使用basic auth中的用户, 不准是root
///     # ssl auth，若使用url中的用户，也不准是root
///     ssh_deny root;
/// }
/// ```
async fn validate_request(request: &Request<()>) -> (Response<()>, crate::error::Result<()>) {
    if request.method() != http::Method::PUT {
        let resp = http::Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(())
            .unwrap();
        let error = io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Missing Authorization header",
        );
        return (resp, Err(error.into()));
    }

    let resp = http::Response::builder()
        .status(StatusCode::OK)
        .body(())
        .unwrap();
    (resp, Ok(()))
}

pub async fn login(
    location: &Arc<Node>,
    conn: Arc<Connection>,
    request: Request<()>,
    recver: RequestStream<RecvStream, Bytes>,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
) -> crate::error::Result<()> {
    let (resp, result) = validate_request(&request).await;
    sender.send_response(resp).await?;
    if let Err(e) = result {
        sender.finish().await?;
        tracing::error!(target: "sshd", "Invalid request: {e}");
        return Err(e);
    }

    let (mux, incomings) = mux::Mux::new(
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

    let localhost = request.uri().host().unwrap_or_default();
    let localhost = localhost.strip_suffix(".genmeta.net").unwrap_or(localhost);
    if let Err(e) = run(conn, mux, localhost, location, incomings).await {
        tracing::error!(target: "sshd", "Server failed: {e:?}");
    }

    Ok(())
}

#[derive(snafu::Snafu, Debug)]
enum SshdError {
    #[snafu(display("Auth for {} failed", username.as_deref().unwrap_or("<unknown>")))]
    Auth {
        source: auth::Error,
        username: Option<String>,
    },
    #[snafu(display("Failed to handle {request:?}"))]
    HandleRequest {
        request: messages::OpenChannel,
        source: Error,
    },
    #[snafu(display("Unexpected request {request:?}, expect {expect}"))]
    UnexpectedRequest {
        expect: &'static str,
        request: messages::OpenChannel,
    },
    #[snafu(display("Failed to receive message"))]
    ReceiveMessage {
        source: mux::ForwardError<io::Error>,
    },
}

async fn run(
    quic_conn: Arc<Connection>,
    mux: Arc<mux::Mux>,
    localhost: &str,
    location: &Node,
    mut incomings: mux::Incomings,
) -> Result<(), SshdError> {
    let user = {
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

        auth::auth(
            &quic_conn,
            &username,
            localhost,
            location,
            auth_channel.sender.framed(),
            auth_channel.recver.framed(),
        )
        .await
        .context(AuthSnafu { username })?
    };

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
                    Err("Auth should only be performed once".into())
                        .context(HandleRequestSnafu { request })
                }
                messages::OpenChannel::Shell { pseudo } => {
                    let pseudo = pseudo.to_owned();
                    let shell = session::shell(&user, pseudo, recver, sender)
                        .await
                        .context(HandleRequestSnafu { request })?;
                    tasks.spawn(shell);
                    Ok(())
                }
                messages::OpenChannel::Exec { pseudo, command } => {
                    let pseudo = pseudo.to_owned();
                    let command = command.to_owned();
                    let exec = session::exec(&user, pseudo, Some(&command), recver, sender)
                        .await
                        .context(HandleRequestSnafu { request })?;
                    tasks.spawn(exec);

                    Ok(())
                }
                messages::OpenChannel::Direct { to: local } => {
                    let forward = forward::accept_forward(sender, recver, local)
                        .await
                        .map_err(Error::from)
                        .context(HandleRequestSnafu { request })?;
                    tasks.spawn(async move {
                        if let Err(error) = forward.await {
                            tracing::error!(target: "local_forward", "Failed to forward data: {error:?}");
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
                        .map_err(Error::from)
                        .context(HandleRequestSnafu { request })?;

                    tasks.spawn(async move {
                    if let Err(accept_error) = accept_forward.await {
                        tracing::error!(target: "remote_forward", "Failed to accept incoming connection to {listen}: {accept_error:?}");
                    }
                });
                    Ok(())
                }
                _ => Err(Error::from("Unexpected request from client"))
                    .context(HandleRequestSnafu { request }),
            }
        };

    while let Some(new_channel) = incomings
        .next()
        .await
        .transpose()
        .context(ReceiveMessageSnafu)?
    {
        if let Err(handle_error) = handle_new_channel(new_channel).await {
            tracing::warn!(target: "sshd", "Failed to handle new request: {handle_error:?}, ending all channels");
            tasks.detach_all();
            return Err(handle_error);
        }
    }
    _ = tasks.join_all().await;

    Ok(())
}
