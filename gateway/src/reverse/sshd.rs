use std::{
    ffi::{CStr, CString},
    io,
    os::{
        fd::AsRawFd,
        unix::{
            io::RawFd,
            prelude::{CommandExt, OwnedFd},
        },
    },
    sync::Arc,
};

use bytes::{Buf, Bytes};
use h3::server::RequestStream;
use h3_shim::{RecvStream, SendStream};
use http::{Request, Response, StatusCode};
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
) -> (Response<()>, Result<(String, String)>) {
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

    // 从 Authorization 头获取认证信息
    let auth_header = match request.headers().get("Authorization") {
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
            return (resp, Err(error.into()));
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
                return (resp, Err(error.into()));
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
            return (resp, Err(error.into()));
        }
    };

    let Some((username, password)) = credentials.split_once(':') else {
        let resp = http::Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(())
            .unwrap();
        let error = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Missing Authorization header",
        );
        return (resp, Err(error.into()));
    };

    let resp = http::Response::builder()
        .status(StatusCode::OK)
        .body(())
        .unwrap();
    (resp, Ok((username.to_string(), password.to_string())))
}
pub async fn login(
    location: &Arc<Node>,
    request: Request<()>,
    recver: RequestStream<RecvStream, Bytes>,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    let (resp, result) = parse_request(location, request).await;
    let (username, password) = match result {
        Ok((username, password)) => {
            sender.send_response(resp).await?;
            (username, password)
        }
        Err(e) => {
            sender.send_response(resp).await?;
            sender.finish().await?;
            tracing::error!("[SSH] Invalid request: {e}");
            return Err(e);
        }
    };
    tracing::debug!("[SSH] Username: {}, Password: {}", username, password);

    // 创建一个伪终端
    let nix::pty::OpenptyResult { master, slave } =
        nix::pty::openpty(None, None).map_err(|errno| map_errno(errno, "Failed to openpty"))?;

    // Fork出子进程
    match unsafe { nix::unistd::fork() }.map_err(|errno| map_errno(errno, "Failed to fork"))? {
        nix::unistd::ForkResult::Parent { child: _ } => {
            // 断开连接
            nix::unistd::close(slave.as_raw_fd())
                .map_err(|errno| map_errno(errno, "Failed to close slave fd in master process"))?;
            // 设置fd为非阻塞模式
            unsafe {
                let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
                libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
            };
            // 启动
            copy_between_pty_and_stream(master, sender, recver).await;
        }
        // 子进程不要抛出错误，出现错误，打印日志，直接exit
        nix::unistd::ForkResult::Child => {
            // 断开连接
            if let Err(errno) = nix::unistd::close(master.as_raw_fd()) {
                tracing::error!("[SSH] Failed to close master fd in child process: {errno:?}");
                std::process::exit(1);
            }
            // 这个函数正常不会返回。返回了就说明出错了
            let exec_error = exec_shell(&username, &password, slave);
            tracing::error!("[SSH] Failed to exec shell: {exec_error:?}");
            std::process::exit(1)
        }
    }

    Ok(())
}

fn exec_shell(username: &str, password: &str, slave: OwnedFd) -> io::Error {
    fn login_tty(slave_fd: RawFd) -> nix::Result<()> {
        nix::unistd::setsid()?;

        nix::ioctl_write_int_bad!(tiocsctty, libc::TIOCSCTTY);
        unsafe { tiocsctty(slave_fd, 0)? };

        nix::unistd::dup2(slave_fd, libc::STDIN_FILENO)?;
        nix::unistd::dup2(slave_fd, libc::STDOUT_FILENO)?;
        nix::unistd::dup2(slave_fd, libc::STDERR_FILENO)?;

        if slave_fd > libc::STDERR_FILENO {
            nix::unistd::close(slave_fd)?;
        }

        Ok(())
    }
    // 设置终端为登录终端
    if let Err(errno) = login_tty(slave.as_raw_fd()) {
        return map_errno(errno, "Failed to login_tty");
    };

    // 寻找用户，验证密码
    let user = CString::new(username).unwrap();
    let pw = unsafe { libc::getpwnam(user.as_ptr()) };
    if pw.is_null() {
        tracing::error!("[SSH] User not found");
        std::process::exit(1)
    }

    // 暂时只支持此验证方式。TODO：支持ssl验证
    if !verify_password(username, password) {
        tracing::error!("[SSH] Authentication failed");
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
    std::process::Command::new(shell)
        .gid(pw.pw_gid)
        .uid(pw.pw_uid)
        .current_dir(home)
        .arg0(shell)
        .arg("--login")
        .env("HOME", home)
        .env("USER", username)
        .env("SHELL", shell)
        .env("TERM", "xterm-256color")
        .exec()
    // 这里不会返回
}

async fn copy_between_pty_and_stream(
    pty_master: OwnedFd,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
    mut recver: RequestStream<RecvStream, Bytes>,
) {
    let async_fd = match AsyncFd::new(pty_master) {
        Ok(async_fd) => Arc::new(async_fd),
        Err(e) => {
            tracing::error!("[SSH] Failed to create async fd: {}", e);
            sender.stop_stream(h3::error::Code::H3_NO_ERROR);
            recver.stop_sending(h3::error::Code::H3_NO_ERROR);
            return; // 直接结束
        }
    };

    // 简易异步IO
    async fn read<F: AsRawFd>(async_fd: &AsyncFd<F>, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut read = async_fd.readable().await?;
            if let Ok(result) = read.try_io(|async_fd| {
                nix::unistd::read(async_fd.as_raw_fd(), buf).map_err(io::Error::from)
            }) {
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
