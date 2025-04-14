use std::{
    ffi::{CStr, CString},
    fs::File,
    io::Write,
    os::fd::{AsRawFd, FromRawFd},
    sync::Arc,
};

use bytes::{Buf, Bytes};
use h3::server::RequestStream;
use h3_shim::{RecvStream, SendStream};
use http::{Request, StatusCode};
use serde::{Deserialize, Serialize};

use crate::{
    error::Result,
    parse::{Node, Value},
};

// 定义客户端与服务器通信的消息结构
#[derive(Serialize, Deserialize, Debug)]
enum TerminalMessage {
    Text(String),
    WindowSize { rows: u16, cols: u16 },
    Signal(i32),
    ControlSequence(String),
    Heartbeat,
}

/// location /ssh {
///     ssh_login basic | ssl; # ssl 需要server级配置ssl_verify_client
///     
///     # 如果是ssl证书认证，可能有多个证书/客户端名字，对应多个用户；
///     # 也可能是一个客户端名字，可以变换多个用户
///     ssh_ssl_user alice.genmeta.net alice; # ssl证书验证有效
///     ssh_ssl_user bob.genmeta.net bob;
///     ssh_ssl_user xxx.genmeta.net $user; # 很多用户都用同一个证书

///     # basic auth就使用basic auth中的用户, 不准是root
///     # ssl auth，若使用url中的用户，也不准是root
///     ssh_deny root;
/// }
pub async fn login(
    location: &Arc<Node>,
    request: Request<()>,
    recver: RequestStream<RecvStream, Bytes>,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    if request.method() != http::Method::PUT {
        let resp = http::Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(())?;
        sender.send_response(resp).await?;
        sender.finish().await?;
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Missing Authorization header",
        )
        .into());
    }

    let ssh_login = if let Some(Value::String(ssl_login)) = location.get("ssh_login") {
        ssl_login
    } else {
        unreachable!();
    };

    if ssh_login == "ssl" {
        let resp = http::Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(())?;
        sender.send_response(resp).await?;
        sender.finish().await?;
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Ssl login is not supported now",
        )
        .into());
    }

    // 从 Authorization 头获取认证信息
    let auth_header = match request.headers().get("Authorization") {
        Some(value) => value.to_str().unwrap_or_default(),
        None => {
            let resp = http::Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(())?;
            sender.send_response(resp).await?;
            sender.finish().await?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Missing Authorization header",
            )
            .into());
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
                    .body(())?;
                sender.send_response(resp).await?;
                sender.finish().await?;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Missing Authorization header",
                )
                .into());
            }
        },
        None => {
            let resp = http::Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(())?;
            sender.send_response(resp).await?;
            sender.finish().await?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Missing Authorization header",
            )
            .into());
        }
    };

    let Some((username, password)) = credentials.split_once(':') else {
        let resp = http::Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(())?;
        sender.send_response(resp).await?;
        sender.finish().await?;
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Missing Authorization header",
        )
        .into());
    };

    tracing::debug!("[SSH] Username: {}, Password: {}", username, password);

    let resp = http::Response::builder().status(StatusCode::OK).body(())?;
    sender.send_response(resp).await?;

    // 创建PTY
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    let mut name_buf = [0u8; 64];
    unsafe {
        libc::openpty(
            &mut master as *mut _,
            &mut slave as *mut _,
            name_buf.as_mut_ptr() as *mut _,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }

    // Fork子进程
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // 子进程
        unsafe {
            libc::close(master);
            libc::login_tty(slave);

            // 设置用户
            let user = CString::new(username).unwrap();
            let pw = libc::getpwnam(user.as_ptr());
            if pw.is_null() {
                println!("User not found");
                libc::exit(1);
            }

            // 暂且先用这种方式校验权限，这种方式不够安全
            // 后续改成quic连接级的证书校验
            if !verify_password(username, password) {
                println!("Authentication failed");
                libc::exit(1);
            }

            // 设置补充组
            libc::initgroups((*pw).pw_name, (*pw).pw_gid as _);
            // 设置gid和uid
            if libc::setgid((*pw).pw_gid) != 0 || libc::setuid((*pw).pw_uid) != 0 {
                eprintln!("Failed to setuid/setgid");
                libc::exit(1);
            }

            // 设置环境变量
            let home = CStr::from_ptr((*pw).pw_dir).to_string_lossy();
            let shell = CStr::from_ptr((*pw).pw_shell).to_string_lossy();
            libc::setenv(
                CString::new("HOME").unwrap().as_ptr(),
                CString::new(home.as_bytes()).unwrap().as_ptr(),
                1,
            );
            libc::setenv(CString::new("USER").unwrap().as_ptr(), user.as_ptr(), 1);
            libc::setenv(
                CString::new("SHELL").unwrap().as_ptr(),
                CString::new(shell.as_bytes()).unwrap().as_ptr(),
                1,
            );

            // 切换工作目录
            if libc::chdir((*pw).pw_dir) != 0 {
                libc::exit(1);
            }

            // 执行shell
            let shell = CString::new(
                CStr::from_ptr((*pw).pw_shell)
                    .to_str()
                    .unwrap_or("/bin/bash"),
            )
            .unwrap();
            libc::execl(
                shell.as_ptr(),
                shell.as_ptr(),
                std::ptr::null::<libc::c_char>() as *const _,
            );
            libc::exit(0);
        }
    }

    // 主进程
    unsafe { libc::close(slave) };
    // 设置master fd为非阻塞模式
    let pty_master = unsafe {
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
        std::fs::File::from_raw_fd(master as _)
    };

    copy_between_pty_and_stream(pty_master, sender, recver).await;

    Ok(())
}

async fn copy_between_pty_and_stream(
    mut pty_master: File,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
    mut recver: RequestStream<RecvStream, Bytes>,
) {
    // 启动读取PTY任务
    let master_fd = pty_master.as_raw_fd();
    let read_task = tokio::spawn(async move {
        let mut read_buf = [0u8; 8192];
        loop {
            match unsafe { libc::read(master_fd, read_buf.as_mut_ptr() as *mut _, read_buf.len()) }
            {
                -1 => {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        tokio::task::yield_now().await;
                        continue;
                    }
                    _ = sender
                        .send_data(Bytes::from(format!("从PTY读取失败: {}", err)))
                        .await
                        .inspect_err(|e| tracing::error!("发送错误消息失败: {}", e));
                    break;
                }
                0 => break, // EOF
                n => {
                    let data = Bytes::copy_from_slice(&read_buf[..n as usize]);
                    if let Err(e) = sender.send_data(data).await {
                        tracing::error!("发送数据失败: {}", e);
                        break;
                    }
                }
            }
        }
        _ = sender
            .finish()
            .await
            .inspect_err(|e| tracing::error!("关闭发送端失败: {}", e));
    });

    // 启动写入PTY任务
    let write_task = tokio::spawn(async move {
        let mut read_buffer = Vec::new();
        while let Ok(Some(data)) = recver.recv_data().await {
            let buf = data.chunk();
            read_buffer.extend_from_slice(buf);

            let mut buf = std::io::Cursor::new(&read_buffer);
            let mut de = serde_json::Deserializer::from_reader(&mut buf);
            loop {
                match TerminalMessage::deserialize(&mut de) {
                    Ok(msg) => {
                        match msg {
                            TerminalMessage::Text(text) => {
                                // 将文本写入PTY
                                if let Err(e) = pty_master.write_all(text.as_bytes()) {
                                    eprintln!("写入PTY失败: {}", e);
                                    recver.stop_sending(h3::error::Code::H3_INTERNAL_ERROR);
                                    return;
                                }
                            }
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
                            TerminalMessage::Signal(signal) => {
                                // 将信号转换为对应的控制字符写入PTY
                                let ctrl_char = match signal {
                                    2 => "\x03", // Ctrl+C (SIGINT)
                                    3 => "\x1A", // Ctrl+Z (SIGTSTP)
                                    _ => return,
                                };
                                if let Err(e) = pty_master.write_all(ctrl_char.as_bytes()) {
                                    eprintln!("写入PTY控制字符失败: {}", e);
                                    recver.stop_sending(h3::error::Code::H3_INTERNAL_ERROR);
                                    return;
                                }
                            }
                            TerminalMessage::ControlSequence(seq) => {
                                // 处理各种控制序列
                                let sequence = match seq.as_str() {
                                    "\x04" => "\x04",       // Ctrl+D (EOF)
                                    "\x7f" => "\x7f",       // Backspace
                                    "\x1b[3~" => "\x1b[3~", // Delete
                                    "\x1b" => "\x1b",       // Esc
                                    "\x1b[A" => "\x1b[A",   // Up
                                    "\x1b[B" => "\x1b[B",   // Down
                                    "\x1b[C" => "\x1b[C",   // Right
                                    "\x1b[D" => "\x1b[D",   // Left
                                    "\x1b[H" => "\x1b[H",   // Home
                                    "\x1b[F" => "\x1b[F",   // End
                                    "\x1b[5~" => "\x1b[5~", // Page Up
                                    "\x1b[6~" => "\x1b[6~", // Page Down
                                    "\t" => "\t",           // Tab
                                    _ => continue,
                                };
                                if let Err(e) = pty_master.write_all(sequence.as_bytes()) {
                                    tracing::error!("写入PTY控制序列失败: {}", e);
                                    recver.stop_sending(h3::error::Code::H3_INTERNAL_ERROR);
                                    return;
                                }
                            }
                            TerminalMessage::Heartbeat => {
                                // 心跳包,不需要处理
                                continue;
                            }
                        }
                    }
                    Err(e) if e.is_eof() => {
                        // 保存未处理完的数据
                        let pos = buf.position() as usize;
                        read_buffer.drain(..pos);
                        break;
                    }
                    Err(e) => {
                        // TODO: fetal error
                        eprintln!("JSON解析错误: {}", e);
                        read_buffer.clear();
                        break;
                    }
                }
            }
        }
    });

    // 等待任意一个任务完成
    tokio::select! {
        _ = read_task => {}
        _ = write_task => {}
    }
}

fn verify_password(username: &str, password: &str) -> bool {
    #[cfg(unix)]
    return {
        let mut auth = pam::Authenticator::with_password("login").expect("Init pam failed");
        auth.get_handler().set_credentials(username, password);
        if let Err(e) = auth.authenticate() {
            println!("Authentication failed: {}", e);
            return false;
        }
        true
    };

    #[allow(unreachable_code)]
    false
}
