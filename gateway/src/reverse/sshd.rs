//! WIP: error handling
use std::{self, sync::Arc};

use bytes::Bytes;
use futures::StreamExt;
use h3::server::RequestStream;
use h3_shim::{RecvStream, SendStream};
use http::{Request, Response, StatusCode};
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
    if let Err(e) = run(mux, localhost, location, incomings).await {
        tracing::error!(target: "sshd", "Server failed: {e:?}");
    }

    Ok(())
}

async fn run(
    mux: Arc<mux::Mux>,
    localhost: &str,
    location: &Node,
    mut incomings: mux::Incomings,
) -> Result<(), Error> {
    let user = {
        let open_auth = match incomings.next().await.transpose() {
            Ok(Some(new_channel)) => new_channel,
            Err(e) => {
                tracing::error!(target: "sshd", "Failed to accept channel: {e:?}");
                return Err(e.into());
            }
            Ok(None) => {
                tracing::error!(target: "sshd", "Failed to auth: no channel");
                return Ok(());
            }
        };

        let messages::OpenChannel::Auth { username } = open_auth.request else {
            return Err(format!("Expect Auth, not {:?}", open_auth.request).into());
        };

        auth::auth(
            &username,
            localhost,
            location,
            open_auth.sender.framed(),
            open_auth.recver.framed(),
        )
        .await?
    };
    let mut tasks = JoinSet::new();
    while let Ok(Some(mux::NewChannel {
        token,
        request,
        sender,
        recver,
    })) = incomings.next().await.transpose()
    {
        match request {
            messages::OpenChannel::Auth { .. } => {
                return Err("Auth should only be preformed once".into());
            }
            messages::OpenChannel::Shell { pseudo } => {
                let pseudo = pseudo.to_owned();
                let shell = session::shell(&user, pseudo, recver, sender).await?;
                tasks.spawn(shell);
            }
            messages::OpenChannel::Exec { pseudo, command } => {
                let pseudo = pseudo.to_owned();
                let command = command.to_owned();
                let exec = session::exec(&user, pseudo, Some(&command), recver, sender).await?;
                tasks.spawn(exec);
            }
            messages::OpenChannel::Direct { to: local } => {
                let forward = forward::accept_forward(sender, recver, local).await?;
                tasks.spawn(async move {
                    if let Err(error) = forward.await {
                        tracing::error!(target: "local_forward", "Failed to accept forward request: {error:?}");
                    }
                });
            }
            messages::OpenChannel::Forward { listen, socks } => {
                let mux = mux.clone();
                tasks.spawn(async move {
                    let result = if !socks {
                        forward::listen_remote_forward(mux, token, sender, recver, listen).await
                    } else {
                        socks::listen_remote_forward(mux, token, sender, recver, listen).await
                    };
                    if let Err(error) = result {
                        tracing::error!(target: "remote_forward", "Failed to accept forward request: {error:?}");
                    }
                });
            }
            _ => return Err(format!("unexpected request {request:?} from Client").into()),
        }
    }
    _ = tasks.join_all().await;
    Ok(())
}
