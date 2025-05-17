use std::{self, fmt::Debug, sync::Arc};

use bytes::Bytes;
use futures::{Sink, SinkExt, Stream, StreamExt, TryStream, TryStreamExt, channel::mpsc};
use h3::server::RequestStream;
use h3_shim::{RecvStream, SendStream};
use http::{Request, Response, StatusCode};
use mux::{ChannelMessage, NewChannel, OpenChannel};
use tokio::io;
use tokio_util::{
    codec,
    io::{CopyToBytes, SinkWriter, StreamReader},
    task::AbortOnDropHandle,
};

mod async_fd;
mod auth;
mod cbor_codec;
mod mux;
#[cfg(feature = "socks")]
mod socks;
mod terminal;

use tracing::Instrument;

use crate::parse::Node;

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
    mut recver: RequestStream<RecvStream, Bytes>,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
) -> crate::error::Result<()> {
    let (resp, result) = validate_request(request).await;
    sender.send_response(resp).await?;
    if let Err(e) = result {
        sender.finish().await?;
        tracing::error!(target: "sshd", "Invalid request: {e}");
        return Err(e);
    }

    let (message_sender, pending_messages) = mpsc::channel::<mux::ChannelMessage>(32);
    let mux = Arc::new(mux::Mux::new(message_sender, mux::Role::Server));

    let send_messages = async {
        let message_sender = codec::FramedWrite::new(
            crate::h3::H3StreamWriter::new(&mut sender),
            cbor_codec::CborEncoder::default(),
        );
        send_messages(message_sender, pending_messages).await;
        _ = sender.finish().await
    };

    let message_handler = mux.clone();
    let (new_channels_sender, new_channels) = mpsc::channel::<NewChannel>(32);
    let recv_messages = async {
        let message_recver = codec::FramedRead::new(
            StreamReader::new(crate::h3::H3StreamReader::new(&mut recver)),
            cbor_codec::CborDecoder::default(),
        );
        recv_messages(message_recver, new_channels_sender, message_handler).await;
        recver.stop_sending(h3::error::Code::H3_NO_ERROR);
    };

    tokio::select! {
        _ = send_messages.in_current_span() => {},
        _ = recv_messages.in_current_span() => {}
        Err(e) = run(location, new_channels) => {
            tracing::error!(target: "sshd", "Server failed: {e:?}");
        }
    }

    Ok(())
}

async fn run(
    location: &Node,
    mut new_channels: impl Stream<Item = NewChannel> + Unpin,
) -> Result<(), Error> {
    let user = {
        let Some(open_auth) = new_channels.next().await else {
            return Ok(());
        };

        let OpenChannel::Auth { username } = open_auth.request() else {
            return Err(format!("expect Auth, not {:?}", open_auth.request()).into());
        };
        let username = username.to_owned();

        let (recver, sender) = open_auth.assume();

        auth::auth(&username, location, sender, recver).await?
    };
    let mut tasks = vec![];
    while let Some(new_channel) = new_channels.next().await {
        match new_channel.request() {
            OpenChannel::Auth { .. } => {
                return Err("Auth should only be preformed once".into());
            }
            OpenChannel::Shell { pseudo } => {
                let pseudo = pseudo.to_owned();
                let (recver, sender) = new_channel.assume();
                tasks.push(AbortOnDropHandle::new(tokio::spawn({
                    let user = user.clone();
                    async move { terminal::shell(&user, pseudo, recver, sender).await }
                })));
            }
            OpenChannel::Exec { pseudo, command } => {
                let pseudo = pseudo.to_owned();
                let command = command.to_owned();
                let (recver, sender) = new_channel.assume();
                tasks.push(AbortOnDropHandle::new(tokio::spawn({
                    let user = user.clone();
                    async move { terminal::exec(&user, pseudo,Some(&command), recver, sender).await }
                })));
            }
            OpenChannel::Socks {} => {
                let (recver, sender) = new_channel.assume::<Bytes, Bytes>();
                tasks.push(AbortOnDropHandle::new(tokio::spawn(async move {
                    let mut reader = StreamReader::new(recver);
                    let mut writer = SinkWriter::new(CopyToBytes::new(sender));
                    Ok(socks::accpet(&mut reader, &mut writer).await?)
                })));
            }
            OpenChannel::Heartbeat {} => todo!(),
        }
    }
    Ok(())
}

async fn send_messages(
    mut message_sender: impl Sink<ChannelMessage, Error: Debug> + Unpin,
    mut pending_messages: impl Stream<Item = ChannelMessage> + Unpin,
) {
    while let Some(message) = pending_messages.next().await {
        if let Err(send_error) = message_sender.send(message).await {
            tracing::error!(target: "sshd", "Failed to send message: {send_error:?}");
            break;
        }
    }
}

async fn recv_messages(
    mut message_recver: impl TryStream<Ok = ChannelMessage, Error: Debug> + Unpin,
    mut new_channels_sender: impl Sink<NewChannel> + Unpin,
    mux: Arc<mux::Mux>,
) {
    loop {
        let message: ChannelMessage = match message_recver.try_next().await {
            Ok(Some(message)) => message,
            Ok(None) => {
                tracing::info!(target: "sshd", "Peer closed the stream");
                break;
            }
            Err(recv_error) => {
                tracing::error!(target: "sshd", "Read from peer error: {recv_error:?}");
                break;
            }
        };

        match mux.receive(message).await {
            Ok(Some(new_channel)) => {
                if (new_channels_sender.send(new_channel).await).is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!(target: "sshd", "Failed to receive message: {e:?}");
            }
        }
    }
}
