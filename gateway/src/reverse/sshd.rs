use std::{
    ffi::{CStr, CString},
    os::{
        fd::AsRawFd,
        unix::prelude::{AsFd, CommandExt},
    },
    sync::Arc,
};

use async_fd::AsyncFd;
use bytes::Bytes;
use futures::{SinkExt, StreamExt, TryStreamExt, channel::mpsc};
use h3::server::RequestStream;
use h3_shim::{RecvStream, SendStream};
use http::{HeaderMap, Request, Response, StatusCode};
use map_sink::MapSinkExt;
use nix::libc;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio_util::{
    codec::{self},
    io::{ReaderStream, StreamReader},
};
use tracing::Instrument;

mod async_fd;
mod cbor_codec;
mod map_sink;
#[cfg(feature = "socks")]
mod socks;

use crate::{
    error::Result,
    h3::{H3StreamReader, H3StreamWriter},
    parse::{Node, Value},
};

// 定义客户端与服务器通信的消息结构
#[derive(Deserialize, Debug)]
enum ClientMessage {
    WindowSize { rows: u16, cols: u16 },
    Terminal { sequence: Bytes },
    // 相比socket addr，一个128位id进行多路复用的开销更小
    Socks(socks::ClientSocksMessage),
    Heartbeat,
}

#[derive(Serialize, Debug)]
enum ServerMessage {
    Terminal { sequence: Bytes },
    Socks(socks::ServerSocksMessage),
    Heartbeat,
}

// TODO：支持ssl验证
#[derive(Debug)]
enum Authorization {
    Basic {
        username: String,
        password: Option<String>,
    },
}

fn map_errno(errno: nix::Error, message: &str) -> std::io::Error {
    let error = std::io::Error::from(errno);
    std::io::Error::new(error.kind(), format!("{message}: {errno}"))
}

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
async fn parse_request(
    location: &Arc<Node>,
    request: Request<()>,
) -> (Response<()>, Result<Authorization>) {
    if request.method() != http::Method::PUT {
        let resp = http::Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(())
            .unwrap();
        let error = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Missing Authorization header",
        );
        return (resp, Err(error.into()));
    }

    let Some(Value::String(ssh_login)) = location.get("ssh_login") else {
        unreachable!();
    };

    let auth = match Authorization::try_from(request.headers()) {
        Ok(auth) => auth,
        Err((resp, error)) => return (resp, Err(error.into())),
    };

    if ssh_login == "ssl" {
        let resp = http::Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(())
            .unwrap();
        let error = std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Ssl login is not supported now",
        );
        return (resp, Err(error.into()));
    }

    if let Some(Value::StringVec(ssh_deny)) = location.get("ssh_deny")
        && ssh_deny.contains(auth.username())
    {
        let resp = http::Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(())
            .unwrap();
        let error =
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "User is not allowed");
        return (resp, Err(error.into()));
    }

    let resp = http::Response::builder()
        .status(StatusCode::OK)
        .body(())
        .unwrap();
    (resp, Ok(auth))
}

pub async fn login(
    location: &Arc<Node>,
    request: Request<()>,
    mut recver: RequestStream<RecvStream, Bytes>,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    let (resp, result) = parse_request(location, request).await;
    let auth = match result {
        Ok(auth) => {
            sender.send_response(resp).await?;
            auth
        }
        Err(e) => {
            sender.send_response(resp).await?;
            sender.finish().await?;
            tracing::error!(target: "sshd", "Invalid request: {e}");
            return Err(e);
        }
    };
    tracing::debug!(target: "sshd", "Authorization: {auth:?}");

    // 创建一个伪终端，frok子进程，设置终端为登录终端
    match unsafe { nix::pty::forkpty(None, None) }
        .map_err(|errno| map_errno(errno, "Failed to forkpty"))?
    {
        nix::pty::ForkptyResult::Parent { child: _, master } => match AsyncFd::new(master) {
            Ok(async_fd) => {
                tracing::info!(target: "sshd", "Begin copy between pty and stream");

                run(async_fd, sender, recver).await;
            }
            Err(e) => {
                tracing::error!(target: "sshd", "Failed to init master fd: {e}");
                sender.stop_stream(h3::error::Code::H3_NO_ERROR);
                recver.stop_sending(h3::error::Code::H3_NO_ERROR);
                return Err(e.into());
            }
        },
        // 子进程不要抛出错误，出现错误，打印日志，直接exit
        // 子进程的内容会被转发到cliet，所以使用eprintln
        nix::pty::ForkptyResult::Child => exec_shell(&auth),
    }

    Ok(())
}

fn exec_shell(auth: &Authorization) -> ! {
    // 寻找用户，验证密码
    let user = CString::new(auth.username().to_owned()).unwrap();
    let pw = unsafe { libc::getpwnam(user.as_ptr()) };
    if pw.is_null() {
        eprintln!("User {} not found", auth.username());
        std::process::exit(1)
    }

    if !auth.authorize() {
        eprintln!("Authentication failed!");
        std::process::exit(1)
    }

    // 获取登陆Shell。AI: 在unix，CStr一定是utf-8
    let pw = unsafe { &*pw };
    let shell = unsafe { CStr::from_ptr(pw.pw_shell) }
        .to_str()
        .expect("unreachable");
    let home = unsafe { CStr::from_ptr(pw.pw_dir) }
        .to_str()
        .expect("unreachable");

    // 设置环境变量，设置uid，gid，调用exec等操作由标准库完成
    // 这里不会返回，除非exec失败
    let exec_error = std::process::Command::new(shell)
        .gid(pw.pw_gid)
        .uid(pw.pw_uid)
        .current_dir(home)
        .arg0(shell)
        .arg("--login")
        .env("HOME", home)
        .env("USER", auth.username())
        .env("SHELL", shell)
        .env("TERM", "xterm-256color")
        .exec();

    eprintln!("Server internal error(Failed to exec shell: {exec_error:?})");
    std::process::exit(1)
}

async fn run(
    pty_master: AsyncFd,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
    mut recver: RequestStream<RecvStream, Bytes>,
) {
    let (pty_read_half, mut pty_write_half) = pty_master.split();

    let (mut message_sender, mut pending_messages) = mpsc::channel(32);

    let send_messages = async move {
        let mut sender = codec::FramedWrite::new(
            H3StreamWriter::new(&mut sender),
            cbor_codec::CborEncoder::default(),
        );
        while let Some(message) = pending_messages.next().await {
            if let Err(send_error) = sender.send(message).await {
                tracing::error!(target: "sshd", "Failed to send message: {send_error:?}");
                break;
            }
        }
        if let Err(close_error) = sender.close().await {
            tracing::error!(target: "sshd", "Failed to close stream: {close_error:?}");
        }
    };

    let socks_server = socks::SocksServer::default();

    // 从pty读取，通过stream发送到client
    let mut terminal_message_sender = message_sender.clone();
    let send_terminal = async move {
        let mut pty_read_stream = ReaderStream::new(pty_read_half);
        loop {
            match pty_read_stream.try_next().await {
                Ok(Some(sequence)) => {
                    let message = ServerMessage::Terminal { sequence };
                    if terminal_message_sender.send(message).await.is_err() {
                        tracing::debug!(target: "sshd", "Failed to send data to peer");
                        break;
                    }
                }
                Ok(None) => {
                    tracing::debug!(target: "sshd", "Read PTY EOF");
                    break;
                }
                Err(e) => {
                    tracing::debug!(target: "sshd", "Failed to read from PTY: {}", e);
                    break;
                }
            }
        }
        terminal_message_sender.close_channel();
    };

    // 解析来自client的信息
    let recv_messages = async move {
        let mut messages_reader = codec::FramedRead::new(
            StreamReader::new(H3StreamReader::new(&mut recver)),
            cbor_codec::CborDecoder::default(),
        );
        loop {
            let message: ClientMessage = match messages_reader.try_next().await {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(de_error) => {
                    tracing::debug!(target: "sshd", "Failed to deserialize message: {de_error}. aborting");
                    break;
                }
            };
            match message {
                ClientMessage::WindowSize { rows, cols } => {
                    // 设置PTY窗口大小
                    unsafe {
                        let winsz = libc::winsize {
                            ws_row: rows,
                            ws_col: cols,
                            ws_xpixel: 0,
                            ws_ypixel: 0,
                        };
                        libc::ioctl(pty_write_half.as_fd().as_raw_fd(), libc::TIOCSWINSZ, &winsz);
                    }
                }
                // Shell
                ClientMessage::Terminal { sequence } => {
                    // 发送数据到shell
                    if let Err(e) = pty_write_half.write_all(&sequence).await {
                        tracing::debug!(target: "sshd", "Failed to write sequence to PTY: {e}");
                        break;
                    }
                }
                ClientMessage::Socks(socks) => match socks {
                    socks::ClientSocksMessage::Init { token } => {
                        let socks_message_sender =
                            message_sender
                                .clone()
                                .mapped(|data: socks::ServerSocksMessage| {
                                    Ok(ServerMessage::Socks(data))
                                });
                        socks_server.accpet(token, socks_message_sender)
                    }
                    socks::ClientSocksMessage::Data { token, data } => {
                        socks_server.receive(token, data).await;
                    }
                    socks::ClientSocksMessage::Finish { token } => {
                        socks_server.close(token);
                    }
                },
                ClientMessage::Heartbeat => {
                    // 心跳包，返回一个心跳包
                    if let Err(error) = message_sender.send(ServerMessage::Heartbeat).await {
                        tracing::warn!(target: "sshd", "Failed to send heartbeat to peer: {error:?}");
                        break;
                    }
                }
            }
        }
        recver.stop_sending(h3::error::Code::H3_NO_ERROR);
    };

    tokio::select! {
        _ = tokio::spawn(send_messages.in_current_span()) => {},
        _ = tokio::spawn(send_terminal.in_current_span()) => {},
        _ = tokio::spawn(recv_messages.in_current_span()) => {}
    }
}

impl TryFrom<&HeaderMap> for Authorization {
    type Error = (Response<()>, std::io::Error);

    fn try_from(headers: &HeaderMap) -> std::result::Result<Self, Self::Error> {
        let auth_header = match headers.get("Authorization") {
            Some(value) => value.to_str().unwrap_or_default(),
            None => {
                let resp = http::Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(())
                    .unwrap();
                let error = std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Missing Authorization header",
                );
                return Err((resp, error));
            }
        };

        // 解析 Basic Auth
        use base64::Engine;
        let credentials = match auth_header.strip_prefix("Basic ") {
            Some(b64) => match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(_) => {
                    let resp = http::Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(())
                        .unwrap();
                    let error = std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "Missing Authorization header",
                    );
                    return Err((resp, error));
                }
            },
            None => {
                let resp = http::Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(())
                    .unwrap();
                let error = std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Missing Authorization header",
                );
                return Err((resp, error));
            }
        };

        Ok(match credentials.split_once(':') {
            Some((username, password)) => Self::Basic {
                username: username.to_owned(),
                password: Some(password.to_owned()),
            },
            None => Self::Basic {
                username: credentials,
                password: None,
            },
        })
    }
}

impl Authorization {
    fn username(&self) -> &String {
        match self {
            Authorization::Basic { username, .. } => username,
        }
    }

    fn authorize(&self) -> bool {
        match self {
            Authorization::Basic {
                username,
                password: None,
            } => {
                const MAX_RETRIES: usize = 3;
                for i in 0..MAX_RETRIES {
                    match rpassword::prompt_password(format!(
                        "Please input password for {username}: "
                    )) {
                        Ok(password) => match verify_password(username, &password) {
                            true => return true,
                            false if i == MAX_RETRIES - 1 => break,
                            false => {
                                eprintln!("Authentication failed! Try again.");
                                continue;
                            }
                        },
                        Err(e) => {
                            eprintln!("Failed to read password: {e}");
                            return false;
                        }
                    }
                }
                false
            }
            Authorization::Basic {
                password: Some(password),
                ..
            } => verify_password(self.username(), password),
        }
    }
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
