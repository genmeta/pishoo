use std::{self, sync::Arc};

use bytes::Bytes;
use futures::{TryStream, TryStreamExt};
use h3::server::RequestStream;
use h3_shim::{RecvStream, SendStream};
use http::{Request, Response, StatusCode};
use mux::{NewChannel, OpenChannel};
use tokio::{
    io::{self, AsyncWriteExt},
    task::JoinSet,
};
use tokio_util::{
    codec,
    io::{CopyToBytes, SinkWriter, StreamReader},
};

mod async_fd;
mod auth;
mod cbor_codec;
mod mux;
#[cfg(feature = "socks")]
mod socks;
mod terminal;

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
async fn validate_request(request: Request<()>) -> (Response<()>, crate::error::Result<()>) {
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
    let (resp, result) = validate_request(request).await;
    sender.send_response(resp).await?;
    if let Err(e) = result {
        sender.finish().await?;
        tracing::error!(target: "sshd", "Invalid request: {e}");
        return Err(e);
    }

    let (_mux, incomings) = mux::Mux::new(
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

    if let Err(e) = run(location, incomings).await {
        tracing::error!(target: "sshd", "Server failed: {e:?}");
    }

    Ok(())
}

async fn run(
    location: &Node,
    mut new_channels: impl TryStream<Ok = NewChannel> + Unpin,
) -> Result<(), Error> {
    let user = {
        let Ok(Some(open_auth)) = new_channels.try_next().await else {
            return Ok(());
        };

        let OpenChannel::Auth { username } = open_auth.request() else {
            return Err(format!("expect Auth, not {:?}", open_auth.request()).into());
        };
        let username = username.to_owned();

        let (recver, sender) = open_auth.assume();

        auth::auth(&username, location, sender, recver).await?
    };
    let mut tasks = JoinSet::new();
    while let Ok(Some(new_channel)) = new_channels.try_next().await {
        match new_channel.request() {
            OpenChannel::Auth { .. } => {
                return Err("Auth should only be preformed once".into());
            }
            OpenChannel::Shell { pseudo } => {
                let pseudo = pseudo.to_owned();
                let (recver, sender) = new_channel.assume();
                let shell = terminal::shell(&user, pseudo, recver, sender).await?;
                tasks.spawn(shell);
            }
            OpenChannel::Exec { pseudo, command } => {
                let pseudo = pseudo.to_owned();
                let command = command.to_owned();
                let (recver, sender) = new_channel.assume();
                let exec = terminal::exec(&user, pseudo, Some(&command), recver, sender).await?;
                tasks.spawn(exec);
            }
            OpenChannel::Socks {} => {
                let (recver, sender) = new_channel.assume::<Bytes, Bytes>();
                tasks.spawn(async move {
                    let mut reader = StreamReader::new(recver);
                    let mut writer = SinkWriter::new(CopyToBytes::new(sender));
                    // 除了io错误还可能是socks协议违背/不支持之类的。这些属于单个Channel，错误不扩散。
                    if let Err(failed) = socks::accpet(&mut reader, &mut writer).await {
                        tracing::error!(target: "sshd", "Failed to accept socks5 request: {failed:?}");
                    }
                    _ = writer.shutdown().await
                });
            }
            OpenChannel::Heartbeat {} => todo!(),
        }
    }
    _ = tasks.join_all().await;
    Ok(())
}
