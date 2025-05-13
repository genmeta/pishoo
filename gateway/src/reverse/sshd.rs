use std::{
    ffi::{CStr, CString},
    io,
    os::{
        fd::{AsFd, AsRawFd},
        unix::prelude::CommandExt,
    },
    sync::Arc,
};

use bytes::{Buf, Bytes};
use h3::server::RequestStream;
use h3_shim::{RecvStream, SendStream};
use http::{HeaderMap, Request, Response, StatusCode};
use nix::libc;
use serde::{Deserialize, Serialize};
use tokio::io::unix::AsyncFd;
use tracing::Instrument;

use crate::{
    error::Result,
    parse::{Node, Value},
};

// 定义客户端与服务器通信的消息结构
#[derive(Serialize, Deserialize, Debug)]
enum TerminalMessage {
    WindowSize { rows: u16, cols: u16 },
    ControlSequence(String),
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
            tracing::error!("[SSH] Invalid request: {e}");
            return Err(e);
        }
    };
    tracing::debug!("[SSH] Authorization: {auth:?}");

    // 创建一个伪终端，frok子进程，设置终端为登录终端
    match unsafe { nix::pty::forkpty(None, None) }
        .map_err(|errno| map_errno(errno, "Failed to forkpty"))?
    {
        nix::pty::ForkptyResult::Parent { child: _, master } => {
            let init_master_fd = async {
                // 设置fd为非阻塞模式
                let flags = nix::fcntl::fcntl(master.as_fd(), nix::fcntl::F_GETFL)
                    .map_err(|errno| map_errno(errno, "Failed to get master fd flags"))?;
                let flags =
                    nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK;
                nix::fcntl::fcntl(master.as_fd(), nix::fcntl::F_SETFL(flags))
                    .map_err(|errno| map_errno(errno, "Failed to set master fd flags"))?;

                // 创建async_fd
                AsyncFd::new(master).map_err(|e| {
                    io::Error::new(e.kind(), format!("Failed to create async fd: {e}"))
                })
            };
            match init_master_fd.await {
                Ok(async_fd) => {
                    copy_between_pty_and_stream(async_fd, sender, recver).await;
                }
                Err(e) => {
                    tracing::error!("[SSH] Failed to init master fd: {e}");
                    sender.stop_stream(h3::error::Code::H3_NO_ERROR);
                    recver.stop_sending(h3::error::Code::H3_NO_ERROR);
                    return Err(e.into());
                }
            }
        }
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
        eprintln!("User not found");
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

async fn copy_between_pty_and_stream<F: AsRawFd + Send + Sync + 'static>(
    pty_master: AsyncFd<F>,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
    mut recver: RequestStream<RecvStream, Bytes>,
) {
    // 简易异步IO
    async fn read<F: AsRawFd>(async_fd: &AsyncFd<F>, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut read = async_fd.readable().await?;
            if let Ok(result) =
                read.try_io(|async_fd| nix::unistd::read(async_fd, buf).map_err(io::Error::from))
            {
                return result;
            }
        }
    }

    async fn write<F: AsRawFd>(async_fd: &AsyncFd<F>, buf: &[u8]) -> io::Result<usize> {
        loop {
            let mut write = async_fd.writable().await?;
            if let Ok(result) =
                write.try_io(|async_fd| nix::unistd::write(async_fd, buf).map_err(io::Error::from))
            {
                return result;
            }
        }
    }

    async fn write_all<F: AsRawFd>(async_fd: &AsyncFd<F>, buf: &[u8]) -> io::Result<()> {
        let mut remaining = buf;
        while !remaining.is_empty() {
            let written = write(async_fd, remaining).await?;
            remaining = &remaining[written..];
        }
        Ok(())
    }

    let async_fd = Arc::new(pty_master);

    // 从pty读取，通过stream发送到client
    let pty_master = Arc::clone(&async_fd);
    let read_task = tokio::spawn(
        async move {
            let mut read_buf = [0u8; 8192];
            loop {
                match read(&pty_master, &mut read_buf).await {
                    Ok(0) => {
                        tracing::debug!("[SSH] Read PTY EOF");
                        break;
                    }
                    Ok(n) => {
                        let data = Bytes::copy_from_slice(&read_buf[..n]);
                        if sender.send_data(data).await.is_err() {
                            tracing::debug!("[SSH] Failed to send data to peer");
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!("[SSH] Failed to read from PTY: {}", e);
                        break;
                    }
                }
            }
            _ = sender.finish().await
        }
        .in_current_span(),
    );

    // 解析来自client的信息，通过pty发送到shell
    let pty_master = Arc::clone(&async_fd);
    let write_task = tokio::spawn(
        async move {
            let mut read_buf = vec![];
            'receive: while let Ok(Some(mut data)) = recver.recv_data().await {
                while data.remaining() > 0 {
                    let chunk = data.chunk();
                    read_buf.extend_from_slice(chunk);
                    data.advance(chunk.len());
                }
                loop {
                    let mut unread = read_buf.as_slice();
                    let mut de = serde_json::Deserializer::from_reader(&mut unread);
                    let message = match TerminalMessage::deserialize(&mut de) {
                        Ok(message) => message,
                        // 暂停解析，等待更多数据
                        Err(e) if e.is_eof() => continue 'receive,
                        // 解析失败
                        Err(e) => {
                            tracing::debug!("[SSH] Failed to deserialize message: {e}");
                            break 'receive;
                        }
                    };

                    // 解析成功，移除被读取的数据
                    read_buf.drain(..read_buf.len() - unread.len());
                    match message {
                        TerminalMessage::WindowSize { rows, cols } => {
                            // 设置PTY窗口大小
                            unsafe {
                                let winsz = libc::winsize {
                                    ws_row: rows,
                                    ws_col: cols,
                                    ws_xpixel: 0,
                                    ws_ypixel: 0,
                                };
                                libc::ioctl(pty_master.as_raw_fd(), libc::TIOCSWINSZ, &winsz);
                            }
                        }
                        TerminalMessage::ControlSequence(sequence) => {
                            if let Err(e) = write_all(&pty_master, sequence.as_bytes()).await {
                                tracing::debug!("[SSH] Failed to write sequence to PTY: {e}");
                                break 'receive;
                            }
                        }
                        TerminalMessage::Heartbeat => {
                            // 心跳包,不需要处理
                            continue;
                        }
                    }
                }
            }
            recver.stop_sending(h3::error::Code::H3_NO_ERROR);
        }
        .in_current_span(),
    );

    tokio::select! {
        _ = read_task => {},
        _ = write_task => {}
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
        tracing::debug!("[SSH] Verifying password {password} for {username}");
        let mut auth = pam::Authenticator::with_password("login").expect("Init pam failed");
        auth.get_handler().set_credentials(username, password);
        auth.authenticate().is_ok()
    };

    #[allow(unreachable_code)]
    false
}
